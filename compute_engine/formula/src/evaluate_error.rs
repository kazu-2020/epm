use std::fmt;

use dimension::{AttributeGlobalId, Dimension, DimensionGlobalId, Dimensions};
use measure::{MeasureError, MeasureGlobalId};

#[derive(Debug, PartialEq)]
pub enum EvaluateError {
    ResultAxesChanged(MeasureGlobalId),
    MissingMeasure(MeasureGlobalId),
    MeasureAxesChanged(MeasureGlobalId),
    MeasurePackingChanged(MeasureGlobalId),
    PackingTooWide(Vec<DimensionGlobalId>),
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

pub fn required_dimension(
    dimensions: &Dimensions,
    global_id: DimensionGlobalId,
) -> Result<&Dimension, EvaluateError> {
    dimensions
        .get(global_id)
        .ok_or(EvaluateError::MissingDimension(global_id))
}
