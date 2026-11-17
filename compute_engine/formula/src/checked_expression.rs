use std::collections::BTreeSet;

use dimension::{AttributeGlobalId, DimensionGlobalId, MemberIndex};
use measure::MeasureGlobalId;

use crate::expression::{BinaryOperator, FilteredAxis};
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
        global_id: MeasureGlobalId,
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
        filtered_axis: FilteredAxis,
    },
}

impl CheckedNode {
    #[must_use]
    pub fn axes(&self) -> BTreeSet<DimensionGlobalId> {
        match self {
            CheckedNode::Number(_) => BTreeSet::new(),
            CheckedNode::Measure { axes, .. } => axes.iter().copied().collect(),
            CheckedNode::Terms(terms) => terms.iter().flat_map(|term| term.node.axes()).collect(),
            CheckedNode::Product { left, right, .. } => {
                left.axes().into_iter().chain(right.axes()).collect()
            }
            CheckedNode::Sum { by, .. } => by
                .iter()
                .map(|axis| match *axis {
                    CheckedAxis::Dimension(dimension) => dimension,
                    CheckedAxis::Attribute { target, .. } => target,
                })
                .collect(),
            CheckedNode::Filter {
                value,
                dimension,
                filtered_axis,
                ..
            } => {
                let mut axes = value.axes();
                if *filtered_axis == FilteredAxis::Drop {
                    axes.remove(dimension);
                }
                axes
            }
        }
    }
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

    #[must_use]
    pub fn sources(&self) -> BTreeSet<MeasureGlobalId> {
        let mut sources = BTreeSet::new();
        collect_sources(&self.root, &mut sources);
        sources
    }
}

fn collect_sources(node: &CheckedNode, sources: &mut BTreeSet<MeasureGlobalId>) {
    match node {
        CheckedNode::Number(_) => {}
        CheckedNode::Measure { global_id, .. } => {
            sources.insert(*global_id);
        }
        CheckedNode::Terms(terms) => {
            for term in terms {
                collect_sources(&term.node, sources);
            }
        }
        CheckedNode::Product { left, right, .. } => {
            collect_sources(left, sources);
            collect_sources(right, sources);
        }
        CheckedNode::Sum { value, .. } | CheckedNode::Filter { value, .. } => {
            collect_sources(value, sources);
        }
    }
}

fn node(expression: &ResolvedExpression) -> Option<CheckedNode> {
    Some(match &expression.kind {
        ResolvedExpressionKind::Number(value) => CheckedNode::Number(*value),
        ResolvedExpressionKind::Measure { global_id, axes } => CheckedNode::Measure {
            global_id: *global_id,
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
            filtered_axis,
        } => CheckedNode::Filter {
            value: Box::new(node(value)?),
            dimension: *dimension,
            members: members.clone(),
            filtered_axis: *filtered_axis,
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
