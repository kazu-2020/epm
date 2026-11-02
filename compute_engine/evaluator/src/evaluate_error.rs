use std::fmt;

use dimension::{AttributeUlid, DimensionUlid};
use formula::AxisError;
use measure::{MeasureError, MeasureUlid};

#[derive(Debug, PartialEq)]
pub enum EvaluateError {
    Unresolved,
    Axes(Vec<AxisError>),
    MissingMeasure(MeasureUlid),
    MeasureAxesChanged(MeasureUlid),
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
