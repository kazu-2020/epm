use std::fmt;

use dimension::{AttributeUlid, DimensionUlid};
use measure::{MeasureError, MeasureUlid};

#[derive(Debug, PartialEq)]
pub enum EvaluateError {
    ResultAxesChanged(MeasureUlid),
    MissingMeasure(MeasureUlid),
    MeasureAxesChanged(MeasureUlid),
    MeasureLayoutChanged(MeasureUlid),
    MissingDimension(DimensionUlid),
    MissingAttribute(AttributeUlid),
    InvalidResult(MeasureError),
}

impl fmt::Display for EvaluateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for EvaluateError {}
