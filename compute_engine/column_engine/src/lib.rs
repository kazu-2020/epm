mod columns;
mod region;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use dimension::{Attribute, Dimension, MemberId};
use evaluator::EvaluateError;
use formula::{BinaryOperator, CheckedAxis, CheckedExpression, CheckedNode};
use measure::{Delta, Measure, MeasureDefinition, MeasureUlid};

use crate::columns::{ByAxis, Columns, find_dimension, layout_of};
pub use crate::region::Region;

pub fn evaluate(
    expression: &CheckedExpression,
    result: &MeasureDefinition,
    measures: &[&Measure],
    dimensions: &[&Dimension],
) -> Result<Measure, EvaluateError> {
    evaluate_in(expression, result, measures, dimensions, &Region::default())?
        .into_measure(result.clone(), dimensions)
        .map_err(EvaluateError::InvalidResult)
}

pub fn edit(
    measures: &mut [Measure],
    edited: MeasureUlid,
    delta: Delta,
    formulas: &[(MeasureUlid, &CheckedExpression)],
    dimensions: &[&Dimension],
) -> Result<(), EvaluateError> {
    // Not edited in place: an error part way would leave results that disagree with their inputs.
    let mut next = measures.to_vec();
    let mut changed = HashMap::new();
    apply(&mut next, edited, delta, &mut changed, dimensions)?;
    for &(result, expression) in formulas {
        let Some(region) = changed_region(expression, &changed, dimensions)? else {
            continue;
        };
        let references: Vec<&Measure> = next.iter().collect();
        let output = position(&next, result)?;
        let delta = recalculate(expression, &next[output], &region, &references, dimensions)?;
        apply(&mut next, result, delta, &mut changed, dimensions)?;
    }
    measures.swap_with_slice(&mut next);
    Ok(())
}

fn apply(
    measures: &mut [Measure],
    ulid: MeasureUlid,
    delta: Delta,
    changed: &mut HashMap<MeasureUlid, Region>,
    dimensions: &[&Dimension],
) -> Result<(), EvaluateError> {
    let index = position(measures, ulid)?;
    let axes = &measures[index].definition().axes;
    if let Some(mut region) =
        Region::of_changes(axes, delta.changes().map(|(coordinate, _)| coordinate))
    {
        for &axis in axes {
            if region.members(axis).map(BTreeSet::len)
                == Some(find_dimension(dimensions, axis)?.len() as usize)
            {
                region = region.without(axis);
            }
        }
        changed.insert(ulid, region);
    }
    measures[index] = measures[index]
        .apply(delta)
        .map_err(EvaluateError::InvalidResult)?;
    Ok(())
}

fn position(measures: &[Measure], ulid: MeasureUlid) -> Result<usize, EvaluateError> {
    measures
        .iter()
        .position(|measure| measure.ulid() == ulid)
        .ok_or(EvaluateError::MissingMeasure(ulid))
}

pub fn changed_region(
    expression: &CheckedExpression,
    changed: &HashMap<MeasureUlid, Region>,
    dimensions: &[&Dimension],
) -> Result<Option<Region>, EvaluateError> {
    changed_in(expression.root(), changed, dimensions)
}

pub fn recalculate(
    expression: &CheckedExpression,
    result: &Measure,
    region: &Region,
    measures: &[&Measure],
    dimensions: &[&Dimension],
) -> Result<Delta, EvaluateError> {
    let definition = result.definition();
    check_layout(result, dimensions)?;
    let new = evaluate_in(expression, definition, measures, dimensions, region)?;
    let old = Columns::from_measure_in(result, region);
    let positions: Vec<usize> = definition
        .axes
        .iter()
        .map(|&axis| new.position(axis))
        .collect();
    let mut delta = result.delta();
    for (coordinate, value) in new.changes_from(&old) {
        let coordinate: Vec<MemberId> = positions
            .iter()
            .map(|&position| MemberId(coordinate[position]))
            .collect();
        match value {
            Some(value) => delta.set(&coordinate, value),
            None => delta.delete(&coordinate),
        }
        .map_err(EvaluateError::InvalidResult)?;
    }
    Ok(delta)
}

fn evaluate_in(
    expression: &CheckedExpression,
    result: &MeasureDefinition,
    measures: &[&Measure],
    dimensions: &[&Dimension],
    region: &Region,
) -> Result<Columns, EvaluateError> {
    if expression.result_axes() != result.axes {
        return Err(EvaluateError::ResultAxesChanged(result.ulid));
    }
    for &axis in &result.axes {
        find_dimension(dimensions, axis)?;
    }
    let evaluator = Evaluator {
        measures,
        dimensions,
        shared: RefCell::new(repeated(expression.root())),
    };
    Ok(evaluator
        .node(expression.root(), region)?
        .broadcast(&result.axes, dimensions)?
        .restrict(region))
}

fn changed_in(
    node: &CheckedNode,
    changed: &HashMap<MeasureUlid, Region>,
    dimensions: &[&Dimension],
) -> Result<Option<Region>, EvaluateError> {
    Ok(match node {
        CheckedNode::Number(_) => None,
        CheckedNode::Measure { ulid, .. } => changed.get(ulid).cloned(),
        CheckedNode::Negate(operand) => changed_in(operand, changed, dimensions)?,
        CheckedNode::Binary { left, right, .. } => match (
            changed_in(left, changed, dimensions)?,
            changed_in(right, changed, dimensions)?,
        ) {
            (Some(left), Some(right)) => Some(left.union(&right)),
            (left, right) => left.or(right),
        },
        CheckedNode::Filter {
            value,
            dimension,
            members,
            keeps_axis,
        } => changed_in(value, changed, dimensions)?.and_then(|mut region| {
            region.constrain(*dimension, members.iter().map(|member| member.0).collect());
            let region = region.non_empty()?;
            Some(if *keeps_axis {
                region
            } else {
                region.without(*dimension)
            })
        }),
        CheckedNode::Sum { value, by } => {
            let Some(source) = changed_in(value, changed, dimensions)? else {
                return Ok(None);
            };
            let mut region = Region::default();
            for axis in by {
                match *axis {
                    CheckedAxis::Dimension(dimension) => {
                        if let Some(members) = source.members(dimension) {
                            region.constrain(dimension, members.clone());
                        }
                    }
                    CheckedAxis::Attribute {
                        dimension,
                        attribute,
                        target,
                    } => {
                        if let Some(members) = source.members(dimension) {
                            let attribute = find_attribute(dimensions, dimension, attribute)?;
                            region.constrain(
                                target,
                                members
                                    .iter()
                                    .filter_map(|&member| attribute.value(MemberId(member)))
                                    .map(|target| target.0)
                                    .collect(),
                            );
                        }
                    }
                }
            }
            region.non_empty()
        }
    })
}

fn check_layout(measure: &Measure, dimensions: &[&Dimension]) -> Result<(), EvaluateError> {
    if *measure.layout() != layout_of(dimensions, &measure.definition().axes)? {
        return Err(EvaluateError::MeasureLayoutChanged(measure.ulid()));
    }
    Ok(())
}

fn find_attribute<'d>(
    dimensions: &[&'d Dimension],
    dimension: dimension::DimensionUlid,
    attribute: dimension::AttributeUlid,
) -> Result<Attribute<'d>, EvaluateError> {
    find_dimension(dimensions, dimension)?
        .attributes()
        .find(|candidate| candidate.ulid() == attribute)
        .ok_or(EvaluateError::MissingAttribute(attribute))
}

struct Evaluator<'a, 'e> {
    measures: &'a [&'a Measure],
    dimensions: &'a [&'a Dimension],
    shared: RefCell<Vec<Shared<'e>>>,
}

struct Shared<'e> {
    node: &'e CheckedNode,
    uses_left: usize,
    result: Option<(Region, Columns)>,
}

fn operands(node: &CheckedNode) -> Vec<&CheckedNode> {
    match node {
        CheckedNode::Number(_) | CheckedNode::Measure { .. } => Vec::new(),
        CheckedNode::Negate(operand) => vec![operand],
        CheckedNode::Binary {
            operator: BinaryOperator::Add | BinaryOperator::Subtract,
            ..
        } => node.terms().into_iter().map(|(_, term)| term).collect(),
        CheckedNode::Binary { left, right, .. } => vec![left, right],
        CheckedNode::Filter { value, .. } | CheckedNode::Sum { value, .. } => vec![value],
    }
}

fn repeated(root: &CheckedNode) -> Vec<Shared<'_>> {
    fn visit<'e>(node: &'e CheckedNode, seen: &mut Vec<Shared<'e>>) {
        if operands(node).is_empty() {
            return;
        }
        if let Some(entry) = seen.iter_mut().find(|entry| entry.node == node) {
            // Not a visit of the operands again: only the first use evaluates them.
            entry.uses_left += 1;
            return;
        }
        seen.push(Shared {
            node,
            uses_left: 1,
            result: None,
        });
        for operand in operands(node) {
            visit(operand, seen);
        }
    }
    let mut seen = Vec::new();
    visit(root, &mut seen);
    seen.retain(|entry| entry.uses_left > 1);
    seen
}

impl<'a, 'e> Evaluator<'a, 'e> {
    fn node(&self, node: &'e CheckedNode, region: &Region) -> Result<Columns, EvaluateError> {
        let Some(position) = self
            .shared
            .borrow()
            .iter()
            .position(|entry| entry.node == node)
        else {
            return self.compute(node, region);
        };
        let cached = {
            let mut shared = self.shared.borrow_mut();
            let entry = &mut shared[position];
            // Not `-= 1`: an outer shared node that misses on another region evaluates its operands again, so they get more uses than visit counted.
            entry.uses_left = entry.uses_left.saturating_sub(1);
            match &entry.result {
                Some((cached_region, columns)) if cached_region == region => {
                    let columns = columns.clone();
                    if entry.uses_left == 0 {
                        entry.result = None;
                    }
                    Some(columns)
                }
                _ => None,
            }
        };
        if let Some(columns) = cached {
            return Ok(columns);
        }
        let columns = self.compute(node, region)?;
        let mut shared = self.shared.borrow_mut();
        if shared[position].uses_left > 0 {
            shared[position].result = Some((region.clone(), columns.clone()));
        }
        Ok(columns)
    }

    fn compute(&self, node: &'e CheckedNode, region: &Region) -> Result<Columns, EvaluateError> {
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
                check_layout(measure, self.dimensions)?;
                if axes.iter().any(|&axis| region.members(axis).is_some()) {
                    Ok(Columns::from_measure_in(measure, region))
                } else {
                    Ok(Columns::from_measure(measure))
                }
            }
            CheckedNode::Negate(operand) => Ok(self.node(operand, region)?.negate()),
            CheckedNode::Binary {
                operator: BinaryOperator::Add | BinaryOperator::Subtract,
                ..
            } => Columns::add(
                node.terms()
                    .into_iter()
                    .map(|(sign, term)| Ok((sign, self.node(term, region)?)))
                    .collect::<Result<Vec<_>, EvaluateError>>()?,
                self.dimensions,
            ),
            CheckedNode::Binary {
                operator,
                left,
                right,
            } => Columns::product(
                *operator,
                &self.node(left, region)?,
                &self.node(right, region)?,
                self.dimensions,
            ),
            CheckedNode::Filter {
                value,
                dimension,
                members,
                keeps_axis,
            } => {
                let mut region = if *keeps_axis {
                    region.clone()
                } else {
                    region.clone().without(*dimension)
                };
                region.constrain(*dimension, members.iter().map(|member| member.0).collect());
                self.node(value, &region)?
                    .filter(*dimension, members, *keeps_axis, self.dimensions)
            }
            CheckedNode::Sum { value, by } => {
                let value = self.node(value, &self.source_region(by, region)?)?;
                let by = by
                    .iter()
                    .map(|axis| self.by_axis(axis, &value))
                    .collect::<Result<Vec<_>, _>>()?;
                value.sum(&by, self.dimensions)
            }
        }
    }

    fn source_region(&self, by: &[CheckedAxis], region: &Region) -> Result<Region, EvaluateError> {
        let mut source = Region::default();
        for axis in by {
            match *axis {
                CheckedAxis::Dimension(dimension) => {
                    if let Some(members) = region.members(dimension) {
                        source.constrain(dimension, members.clone());
                    }
                }
                CheckedAxis::Attribute {
                    dimension,
                    attribute,
                    target,
                } => {
                    if let Some(targets) = region.members(target) {
                        let attribute = find_attribute(self.dimensions, dimension, attribute)?;
                        let members: BTreeSet<u32> =
                            (0..find_dimension(self.dimensions, dimension)?.len())
                                .filter(|&member| {
                                    attribute
                                        .value(MemberId(member))
                                        .is_some_and(|target| targets.contains(&target.0))
                                })
                                .collect();
                        source.constrain(dimension, members);
                    }
                }
            }
        }
        Ok(source)
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
                attribute: Some(find_attribute(self.dimensions, dimension, attribute)?),
            }),
        }
    }
}
