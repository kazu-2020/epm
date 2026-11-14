use std::fmt;

use arrow_schema::SchemaRef;
use dimension::{DimensionGlobalId, MemberIndex};

use crate::definition::MeasureGlobalId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinateError {
    Length {
        expected: usize,
        found: usize,
    },
    MemberOutOfRange {
        axis: DimensionGlobalId,
        member: MemberIndex,
    },
}

impl fmt::Display for CoordinateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoordinateError::Length { expected, found } => {
                write!(
                    f,
                    "the coordinate needs {expected} members, but it has {found}"
                )
            }
            CoordinateError::MemberOutOfRange { axis, member } => {
                write!(f, "member {} is not in dimension {}", member.0, axis.0)
            }
        }
    }
}

impl std::error::Error for CoordinateError {}

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
    Coordinate(CoordinateError),
    PackingTooWide,
    BitsOutsidePacking(usize),
    UnnormalizedValue(usize),
    UnsortedCoordinate(usize),
    DuplicateCoordinate(usize),
    ForeignDelta(MeasureGlobalId),
    DeltaAxesMismatch,
}

impl From<CoordinateError> for MeasureError {
    fn from(error: CoordinateError) -> Self {
        MeasureError::Coordinate(error)
    }
}

impl fmt::Display for MeasureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for MeasureError {}
