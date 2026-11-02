use dimension::{AttributeUlid, DimensionUlid};
use measure::MeasureUlid;

use crate::expression::BinaryOperator;
use crate::resolved_expression::{ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind};

#[derive(Debug, Clone, PartialEq)]
pub struct CheckedExpression {
    root: CheckedNode,
    result_axes: Vec<DimensionUlid>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckedNode {
    Number(f64),
    Measure {
        ulid: MeasureUlid,
        axes: Vec<DimensionUlid>,
    },
    Negate(Box<CheckedNode>),
    Binary {
        operator: BinaryOperator,
        left: Box<CheckedNode>,
        right: Box<CheckedNode>,
    },
    Sum {
        value: Box<CheckedNode>,
        by: Vec<CheckedAxis>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedAxis {
    Dimension(DimensionUlid),
    Attribute {
        dimension: DimensionUlid,
        attribute: AttributeUlid,
        target: DimensionUlid,
    },
}

impl CheckedExpression {
    pub(crate) fn new(
        expression: &ResolvedExpression,
        result_axes: &[DimensionUlid],
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
    pub fn result_axes(&self) -> &[DimensionUlid] {
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
        ResolvedExpressionKind::Negate(operand) => CheckedNode::Negate(Box::new(node(operand)?)),
        ResolvedExpressionKind::Binary {
            operator,
            left,
            right,
        } => CheckedNode::Binary {
            operator: *operator,
            left: Box::new(node(left)?),
            right: Box::new(node(right)?),
        },
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
        ResolvedExpressionKind::Error => return None,
    })
}
