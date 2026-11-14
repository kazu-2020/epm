use std::ops::Range;

use dimension::{AttributeGlobalId, DimensionGlobalId, MemberIndex};
use measure::MeasureGlobalId;

use crate::expression::BinaryOperator;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExpression {
    pub kind: ResolvedExpressionKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedExpressionKind {
    Number(f64),
    Measure {
        global_id: MeasureGlobalId,
        axes: Vec<DimensionGlobalId>,
    },
    Negate(Box<ResolvedExpression>),
    Binary {
        operator: BinaryOperator,
        left: Box<ResolvedExpression>,
        right: Box<ResolvedExpression>,
    },
    Sum {
        value: Box<ResolvedExpression>,
        by: Vec<ResolvedAxis>,
    },
    Filter {
        value: Box<ResolvedExpression>,
        dimension: DimensionGlobalId,
        members: Vec<MemberIndex>,
        keeps_axis: bool,
    },
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAxis {
    pub kind: ResolvedAxisKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedAxisKind {
    Dimension(DimensionGlobalId),
    Attribute {
        dimension: DimensionGlobalId,
        attribute: AttributeGlobalId,
        target: DimensionGlobalId,
    },
    Unresolved,
}
