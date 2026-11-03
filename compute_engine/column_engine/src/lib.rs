mod columns;

use dimension::Dimension;
use evaluator::EvaluateError;
use formula::{BinaryOperator, CheckedAxis, CheckedExpression, CheckedNode};
use measure::{Measure, MeasureDefinition};

use crate::columns::{ByAxis, Columns, find_dimension};

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
    fn node(&self, node: &CheckedNode) -> Result<Columns, EvaluateError> {
        match node {
            CheckedNode::Number(value) => Ok(Columns::number(*value)),
            CheckedNode::Measure { ulid, axes } => {
                let measure = self
                    .measures
                    .iter()
                    .find(|measure| measure.ulid() == *ulid)
                    .ok_or(EvaluateError::MissingMeasure(*ulid))?;
                if measure.definition().axes != *axes {
                    return Err(EvaluateError::MeasureAxesChanged(*ulid));
                }
                Ok(Columns::from_measure(measure))
            }
            CheckedNode::Negate(operand) => Ok(self.node(operand)?.negate()),
            CheckedNode::Binary {
                operator: BinaryOperator::Add | BinaryOperator::Subtract,
                ..
            } => Columns::add(
                node.terms()
                    .into_iter()
                    .map(|(sign, term)| Ok((sign, self.node(term)?)))
                    .collect::<Result<Vec<_>, EvaluateError>>()?,
                self.dimensions,
            ),
            CheckedNode::Binary {
                operator,
                left,
                right,
            } => Ok(Columns::product(
                *operator,
                &self.node(left)?,
                &self.node(right)?,
            )),
            CheckedNode::Sum { value, by } => {
                let value = self.node(value)?;
                let by = by
                    .iter()
                    .map(|axis| self.by_axis(axis, &value))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(value.sum(&by))
            }
        }
    }

    fn by_axis(&self, axis: &CheckedAxis, value: &Columns) -> Result<ByAxis<'a>, EvaluateError> {
        match *axis {
            CheckedAxis::Dimension(dimension) => Ok(ByAxis {
                output: dimension,
                source: value.position(dimension),
                attribute: None,
            }),
            CheckedAxis::Attribute {
                dimension,
                attribute,
                target,
            } => Ok(ByAxis {
                output: target,
                source: value.position(dimension),
                attribute: Some(
                    find_dimension(self.dimensions, dimension)?
                        .attributes()
                        .find(|candidate| candidate.ulid() == attribute)
                        .ok_or(EvaluateError::MissingAttribute(attribute))?,
                ),
            }),
        }
    }
}
