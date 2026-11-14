mod cells;

use dimension::{Dimension, DimensionGlobalId};
use formula::{CheckedAxis, CheckedExpression, CheckedNode, EvaluateError};
use measure::{Measure, MeasureDefinition};

use crate::cells::{ByAxis, Cells, find_dimension};

pub fn evaluate(
    expression: &CheckedExpression,
    result: &MeasureDefinition,
    measures: &[&Measure],
    dimensions: &[&Dimension],
) -> Result<Measure, EvaluateError> {
    if expression.result_axes() != result.axes {
        return Err(EvaluateError::ResultAxesChanged(result.ulid));
    }
    for &axis in &result.axes {
        find_dimension(dimensions, axis)?;
    }
    let evaluator = Evaluator {
        measures,
        dimensions,
    };
    evaluator
        .node(expression.root())?
        .broadcast(&result.axes, dimensions)?
        .into_measure(result.clone(), dimensions)
        .map_err(EvaluateError::InvalidResult)
}

struct Evaluator<'a> {
    measures: &'a [&'a Measure],
    dimensions: &'a [&'a Dimension],
}

impl<'a> Evaluator<'a> {
    fn node(&self, node: &CheckedNode) -> Result<Cells, EvaluateError> {
        match node {
            CheckedNode::Number(value) => Ok(Cells::number(*value)),
            CheckedNode::Measure { ulid, axes } => {
                let measure = self
                    .measures
                    .iter()
                    .find(|measure| measure.ulid() == *ulid)
                    .ok_or(EvaluateError::MissingMeasure(*ulid))?;
                // Not the resolved axes alone: the columns are laid out by the measure's own axes, which may have changed since the formula was resolved.
                if measure.definition().axes != *axes {
                    return Err(EvaluateError::MeasureAxesChanged(*ulid));
                }
                Ok(Cells::from_measure(measure))
            }
            CheckedNode::Terms(terms) => Cells::add(
                terms
                    .iter()
                    .map(|term| Ok((term.sign, self.node(&term.node)?)))
                    .collect::<Result<Vec<_>, EvaluateError>>()?,
                self.dimensions,
            ),
            CheckedNode::Product {
                operator,
                left,
                right,
            } => Ok(Cells::product(
                *operator,
                self.node(left)?,
                self.node(right)?,
            )),
            CheckedNode::Filter {
                value,
                dimension,
                members,
                keeps_axis,
            } => Ok(self.node(value)?.filter(*dimension, members, *keeps_axis)),
            CheckedNode::Sum { value, by } => {
                let value = self.node(value)?;
                let by = by
                    .iter()
                    .map(|axis| self.by_axis(axis, &value.axes))
                    .collect::<Result<Vec<_>, _>>()?;
                value.sum(&by, self.dimensions)
            }
        }
    }

    fn by_axis(
        &self,
        axis: &CheckedAxis,
        value_axes: &[DimensionGlobalId],
    ) -> Result<ByAxis<'a>, EvaluateError> {
        let position = |dimension| {
            value_axes
                .iter()
                .position(|&axis| axis == dimension)
                .expect("a checked expression has every BY axis on its value")
        };
        match *axis {
            CheckedAxis::Dimension(dimension) => Ok(ByAxis {
                output: dimension,
                source: position(dimension),
                attribute: None,
            }),
            CheckedAxis::Attribute {
                dimension,
                attribute,
                target,
            } => Ok(ByAxis {
                output: target,
                source: position(dimension),
                attribute: Some(
                    find_dimension(self.dimensions, dimension)?
                        .reference_attributes()
                        .find(|candidate| candidate.ulid() == attribute)
                        .ok_or(EvaluateError::MissingAttribute(attribute))?,
                ),
            }),
        }
    }
}
