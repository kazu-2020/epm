use std::fmt;

use arrow_schema::SchemaRef;
use dimension::{DimensionGlobalId, MemberIndex};

use crate::definition::MeasureGlobalId;

#[derive(Clone, Debug, PartialEq)]
pub enum MeasureError {
    NoAxes,
    DuplicateAxis(DimensionGlobalId),
    MissingDimension(DimensionGlobalId),
    SchemaMismatch {
        expected: SchemaRef,
        found: SchemaRef,
    },
    NullInColumn(String),
    MemberOutOfRange {
        axis: DimensionGlobalId,
        member: MemberIndex,
    },
    PackingTooWide,
    BitsOutsidePacking(usize),
    UnnormalizedValue(usize),
    UnsortedCoordinate(usize),
    DuplicateCoordinate(usize),
    CoordinateLength {
        expected: usize,
        found: usize,
    },
    ForeignDelta(MeasureGlobalId),
    DeltaAxesMismatch,
}

impl fmt::Display for MeasureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for MeasureError {}
