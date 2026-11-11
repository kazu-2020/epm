use std::ops::Range;

use dimension::DimensionGlobalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisError {
    pub kind: AxisErrorKind,
    pub dimension: DimensionGlobalId,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisErrorKind {
    ValueLacksAxis,
    AxisNotInResult,
    DuplicateAxis,
    ReducesAxesWithoutAggregation,
}
