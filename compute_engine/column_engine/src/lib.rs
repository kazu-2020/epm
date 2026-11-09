mod columns;
mod region;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use dimension::{Attribute, Dimension, MemberId};
use evaluator::EvaluateError;
use formula::{CheckedAxis, CheckedExpression, CheckedNode};
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
    deltas: impl IntoIterator<Item = (MeasureUlid, Delta)>,
    formulas: &[(MeasureUlid, &CheckedExpression)],
    dimensions: &[&Dimension],
) -> Result<(), EvaluateError> {
    // Not edited in place: an error part way would leave results that disagree with their inputs.
    let mut next = measures.to_vec();
    let mut changed = HashMap::new();
    for (edited, delta) in deltas {
        apply(&mut next, edited, delta, &mut changed, dimensions)?;
    }
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
    mut delta: Delta,
    changed: &mut HashMap<MeasureUlid, Region>,
    dimensions: &[&Dimension],
) -> Result<(), EvaluateError> {
    let index = position(measures, ulid)?;
    let axes = &measures[index].definition().axes;
    if let Some(mut region) = Region::of_keys(axes, measures[index].layout(), delta.keys()) {
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
    let old = Columns::from_measure_in(result, region, dimensions)?;
    let positions: Vec<usize> = definition
        .axes
        .iter()
        .map(|&axis| new.position(axis))
        .collect();
    let (mut keys, mut changes) = new.changes_from(&old);
    if !positions.is_sorted() {
        let layout = new.layout();
        let mut pairs: Vec<(u64, Option<f64>)> = keys
            .iter()
            .zip(changes)
            .map(|(&key, change)| {
                let members = positions
                    .iter()
                    .map(|&position| layout.member(key, position));
                (result.layout().pack(members), change)
            })
            .collect();
        pairs.sort_unstable_by_key(|&(key, _)| key);
        (keys, changes) = pairs.into_iter().unzip();
    }
    result
        .delta_from_sorted(keys, changes)
        .map_err(EvaluateError::InvalidResult)
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
        shared: RefCell::new(Vec::new()),
    };
    let mut shared = Vec::new();
    evaluator.count_uses(expression.root(), region, &mut shared)?;
    shared.retain(|entry| entry.uses_left > 1);
    evaluator.shared.replace(shared);
    let columns = evaluator.node(expression.root(), region)?;
    debug_assert!(
        evaluator
            .shared
            .borrow()
            .iter()
            .all(|entry| entry.uses_left == 0 && entry.result.is_none())
    );
    Ok(columns
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
        CheckedNode::Terms(_) | CheckedNode::Product { .. } => {
            let mut union: Option<Region> = None;
            for operand in operands(node) {
                union = match (union, changed_in(operand, changed, dimensions)?) {
                    (Some(left), Some(right)) => Some(left.union(&right)),
                    (left, right) => left.or(right),
                };
            }
            union
        }
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
    region: Region,
    uses_left: usize,
    result: Option<Columns>,
}

fn operands(node: &CheckedNode) -> Vec<&CheckedNode> {
    match node {
        CheckedNode::Number(_) | CheckedNode::Measure { .. } => Vec::new(),
        CheckedNode::Terms(terms) => terms.iter().map(|term| &term.node).collect(),
        CheckedNode::Product { left, right, .. } => vec![left, right],
        CheckedNode::Filter { value, .. } | CheckedNode::Sum { value, .. } => vec![value],
    }
}

impl<'a, 'e> Evaluator<'a, 'e> {
    fn count_uses(
        &self,
        node: &'e CheckedNode,
        region: &Region,
        seen: &mut Vec<Shared<'e>>,
    ) -> Result<(), EvaluateError> {
        if operands(node).is_empty() {
            return Ok(());
        }
        if let Some(entry) = seen
            .iter_mut()
            .find(|entry| entry.node == node && entry.region == *region)
        {
            entry.uses_left += 1;
            return Ok(());
        }
        seen.push(Shared {
            node,
            region: region.clone(),
            uses_left: 1,
            result: None,
        });
        let operand_region = self.operand_region(node, region)?;
        for operand in operands(node) {
            self.count_uses(operand, &operand_region, seen)?;
        }
        Ok(())
    }

    fn operand_region(&self, node: &CheckedNode, region: &Region) -> Result<Region, EvaluateError> {
        match node {
            CheckedNode::Filter {
                dimension,
                members,
                keeps_axis,
                ..
            } => {
                let mut region = if *keeps_axis {
                    region.clone()
                } else {
                    region.clone().without(*dimension)
                };
                region.constrain(*dimension, members.iter().map(|member| member.0).collect());
                Ok(region)
            }
            CheckedNode::Sum { by, .. } => self.source_region(by, region),
            _ => Ok(region.clone()),
        }
    }

    fn node(&self, node: &'e CheckedNode, region: &Region) -> Result<Columns, EvaluateError> {
        let Some(position) = self
            .shared
            .borrow()
            .iter()
            .position(|entry| entry.node == node && entry.region == *region)
        else {
            return self.compute(node, region);
        };
        let cached = {
            let mut shared = self.shared.borrow_mut();
            let entry = &mut shared[position];
            entry.uses_left -= 1;
            if entry.uses_left == 0 {
                entry.result.take()
            } else {
                entry.result.clone()
            }
        };
        if let Some(columns) = cached {
            return Ok(columns);
        }
        let columns = self.compute(node, region)?;
        let mut shared = self.shared.borrow_mut();
        if shared[position].uses_left > 0 {
            shared[position].result = Some(columns.clone());
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
                    Columns::from_measure_in(measure, region, self.dimensions)
                } else {
                    Columns::from_measure(measure, self.dimensions)
                }
            }
            CheckedNode::Terms(terms) => Columns::add(
                terms
                    .iter()
                    .map(|term| Ok((term.sign, self.node(&term.node, region)?)))
                    .collect::<Result<Vec<_>, EvaluateError>>()?,
                self.dimensions,
            ),
            CheckedNode::Product {
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
            } => self
                .node(value, &self.operand_region(node, region)?)?
                .filter(*dimension, members, *keeps_axis, self.dimensions),
            CheckedNode::Sum { value, by } => {
                let value = self.node(value, &self.operand_region(node, region)?)?;
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
