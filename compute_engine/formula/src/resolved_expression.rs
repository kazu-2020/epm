use std::ops::Range;

use dimension::{AttributeUlid, DimensionUlid};
use tile::TileUlid;

use crate::expression::BinaryOperator;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExpression {
    pub kind: ResolvedExpressionKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedExpressionKind {
    Number(f64),
    Tile {
        ulid: TileUlid,
        axes: Vec<DimensionUlid>,
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
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAxis {
    pub kind: ResolvedAxisKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedAxisKind {
    Dimension(DimensionUlid),
    Attribute {
        dimension: DimensionUlid,
        attribute: AttributeUlid,
        target: DimensionUlid,
    },
    Unresolved,
}
