mod cells;
mod evaluate_error;

use dimension::{Dimension, DimensionUlid};
use formula::{ResolvedAxis, ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind};
use measure::{Measure, MeasureDefinition};

use crate::cells::{ByAxis, Cells, find_dimension};
pub use crate::evaluate_error::EvaluateError;

pub fn evaluate(
    expression: &ResolvedExpression,
    result: &MeasureDefinition,
    measures: &[&Measure],
    dimensions: &[&Dimension],
) -> Result<Measure, EvaluateError> {
    let check = formula::check_axes(expression, &result.axes);
    if !check.errors.is_empty() {
        return Err(EvaluateError::Axes(check.errors));
    }
    let evaluator = Evaluator {
        measures,
        dimensions,
    };
    evaluator
        .expression(expression)?
        .broadcast(&result.axes, dimensions)?
        .into_measure(result.clone(), dimensions)
        .map_err(EvaluateError::InvalidResult)
}

struct Evaluator<'a> {
    measures: &'a [&'a Measure],
    dimensions: &'a [&'a Dimension],
}

impl<'a> Evaluator<'a> {
    fn expression(&self, expression: &ResolvedExpression) -> Result<Cells, EvaluateError> {
        match &expression.kind {
            ResolvedExpressionKind::Number(value) => Ok(Cells::number(*value)),
            ResolvedExpressionKind::Measure { ulid, axes } => {
                let measure = self
                    .measures
                    .iter()
                    .find(|measure| measure.ulid() == *ulid)
                    .ok_or(EvaluateError::MissingMeasure(*ulid))?;
                Ok(Cells::from_measure(axes, measure.compact().base()))
            }
            ResolvedExpressionKind::Negate(operand) => Ok(self.expression(operand)?.negate()),
            ResolvedExpressionKind::Binary {
                operator,
                left,
                right,
            } => Cells::combine(
                *operator,
                self.expression(left)?,
                self.expression(right)?,
                self.dimensions,
            ),
            ResolvedExpressionKind::Sum { value, by } => {
                let value = self.expression(value)?;
                let by = by
                    .iter()
                    .map(|axis| self.by_axis(axis, &value.axes))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(value.sum(&by))
            }
            ResolvedExpressionKind::Error => Err(EvaluateError::Unresolved),
        }
    }

    fn by_axis(
        &self,
        axis: &ResolvedAxis,
        value_axes: &[DimensionUlid],
    ) -> Result<ByAxis<'a>, EvaluateError> {
        let position = |dimension| {
            value_axes
                .iter()
                .position(|&axis| axis == dimension)
                .expect("check_axes rejects a BY axis that the value lacks")
        };
        match axis.kind {
            ResolvedAxisKind::Dimension(dimension) => Ok(ByAxis {
                output: dimension,
                source: position(dimension),
                attribute: None,
            }),
            ResolvedAxisKind::Attribute {
                dimension,
                attribute,
                target,
            } => Ok(ByAxis {
                output: target,
                source: position(dimension),
                attribute: Some(
                    find_dimension(self.dimensions, dimension)?
                        .attributes()
                        .find(|candidate| candidate.ulid() == attribute)
                        .ok_or(EvaluateError::MissingAttribute(attribute))?,
                ),
            }),
            ResolvedAxisKind::Unresolved => Err(EvaluateError::Unresolved),
        }
    }
}
