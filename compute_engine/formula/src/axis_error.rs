use std::ops::Range;

use dimension::DimensionUlid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisError {
    pub kind: AxisErrorKind,
    pub dimension: DimensionUlid,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisErrorKind {
    ValueLacksAxis,
    AxisNotInResult,
    DuplicateAxis,
    ReducesAxesWithoutAggregation,
}
