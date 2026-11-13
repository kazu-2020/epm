use std::fmt;

use dimension::{AttributeGlobalId, DimensionGlobalId};
use measure::{MeasureError, MeasureGlobalId};

#[derive(Debug, PartialEq)]
pub enum EvaluateError {
    ResultAxesChanged(MeasureGlobalId),
    MissingMeasure(MeasureGlobalId),
    MeasureAxesChanged(MeasureGlobalId),
    MeasurePackingChanged(MeasureGlobalId),
    PackingTooWide,
    MissingDimension(DimensionGlobalId),
    MissingAttribute(AttributeGlobalId),
    InvalidResult(MeasureError),
}

impl fmt::Display for EvaluateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for EvaluateError {}
