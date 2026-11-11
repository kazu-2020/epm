use dimension::{AttributeGlobalId, DimensionGlobalId, MemberIndex};
use measure::MeasureGlobalId;

use crate::expression::BinaryOperator;
use crate::resolved_expression::{ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind};

#[derive(Debug, Clone, PartialEq)]
pub struct CheckedExpression {
    root: CheckedNode,
    result_axes: Vec<DimensionGlobalId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckedNode {
    Number(f64),
    Measure {
        ulid: MeasureGlobalId,
        axes: Vec<DimensionGlobalId>,
    },
    Terms(Vec<Term>),
    Product {
        operator: ProductOperator,
        left: Box<CheckedNode>,
        right: Box<CheckedNode>,
    },
    Sum {
        value: Box<CheckedNode>,
        by: Vec<CheckedAxis>,
    },
    Filter {
        value: Box<CheckedNode>,
        dimension: DimensionGlobalId,
        members: Vec<MemberIndex>,
        keeps_axis: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub sign: Sign,
    pub node: CheckedNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductOperator {
    Multiply,
    Divide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    Plus,
    Minus,
}

impl Sign {
    fn flipped(self) -> Sign {
        match self {
            Sign::Plus => Sign::Minus,
            Sign::Minus => Sign::Plus,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedAxis {
    Dimension(DimensionGlobalId),
    Attribute {
        dimension: DimensionGlobalId,
        attribute: AttributeGlobalId,
        target: DimensionGlobalId,
    },
}

impl CheckedExpression {
    pub(crate) fn new(
        expression: &ResolvedExpression,
        result_axes: &[DimensionGlobalId],
    ) -> CheckedExpression {
        CheckedExpression {
            root: node(expression).expect("the checker reports every unresolved node"),
            result_axes: result_axes.to_vec(),
        }
    }

    #[must_use]
    pub fn root(&self) -> &CheckedNode {
        &self.root
    }

    #[must_use]
    pub fn result_axes(&self) -> &[DimensionGlobalId] {
        &self.result_axes
    }
}

fn node(expression: &ResolvedExpression) -> Option<CheckedNode> {
    Some(match &expression.kind {
        ResolvedExpressionKind::Number(value) => CheckedNode::Number(*value),
        ResolvedExpressionKind::Measure { ulid, axes } => CheckedNode::Measure {
            ulid: *ulid,
            axes: axes.clone(),
        },
        ResolvedExpressionKind::Negate(_)
        | ResolvedExpressionKind::Binary {
            operator: BinaryOperator::Add | BinaryOperator::Subtract,
            ..
        } => {
            let mut terms = Vec::new();
            push_terms(expression, Sign::Plus, &mut terms)?;
            CheckedNode::Terms(terms)
        }
        ResolvedExpressionKind::Binary {
            operator: BinaryOperator::Multiply,
            left,
            right,
        } => product(ProductOperator::Multiply, left, right)?,
        ResolvedExpressionKind::Binary {
            operator: BinaryOperator::Divide,
            left,
            right,
        } => product(ProductOperator::Divide, left, right)?,
        ResolvedExpressionKind::Sum { value, by } => CheckedNode::Sum {
            value: Box::new(node(value)?),
            by: by
                .iter()
                .map(|axis| match axis.kind {
                    ResolvedAxisKind::Dimension(dimension) => {
                        Some(CheckedAxis::Dimension(dimension))
                    }
                    ResolvedAxisKind::Attribute {
                        dimension,
                        attribute,
                        target,
                    } => Some(CheckedAxis::Attribute {
                        dimension,
                        attribute,
                        target,
                    }),
                    ResolvedAxisKind::Unresolved => None,
                })
                .collect::<Option<_>>()?,
        },
        ResolvedExpressionKind::Filter {
            value,
            dimension,
            members,
            keeps_axis,
        } => CheckedNode::Filter {
            value: Box::new(node(value)?),
            dimension: *dimension,
            members: members.clone(),
            keeps_axis: *keeps_axis,
        },
        ResolvedExpressionKind::Error => return None,
    })
}

fn product(
    operator: ProductOperator,
    left: &ResolvedExpression,
    right: &ResolvedExpression,
) -> Option<CheckedNode> {
    Some(CheckedNode::Product {
        operator,
        left: Box::new(node(left)?),
        right: Box::new(node(right)?),
    })
}

fn push_terms(expression: &ResolvedExpression, sign: Sign, terms: &mut Vec<Term>) -> Option<()> {
    match &expression.kind {
        ResolvedExpressionKind::Binary {
            operator: operator @ (BinaryOperator::Add | BinaryOperator::Subtract),
            left,
            right,
        } => {
            push_terms(left, sign, terms)?;
            let right_sign = if *operator == BinaryOperator::Add {
                sign
            } else {
                sign.flipped()
            };
            push_terms(right, right_sign, terms)?;
        }
        ResolvedExpressionKind::Negate(operand) => push_terms(operand, sign.flipped(), terms)?,
        _ => terms.push(Term {
            sign,
            node: node(expression)?,
        }),
    }
    Some(())
}
