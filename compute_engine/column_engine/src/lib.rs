mod columns;
mod region;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use dimension::{Dimensions, MemberIndex, ReferenceAttribute, usize_from};
use formula::{
    CheckedAxis, CheckedExpression, CheckedNode, EvaluateError, FilteredAxis, required_dimension,
};
use measure::{Delta, Measure, MeasureDefinition, MeasureGlobalId};

use crate::columns::{ByAxis, Columns, packing_of};
use crate::region::Region;

pub fn evaluate(
    expression: &CheckedExpression,
    result: &MeasureDefinition,
    measures: &[&Measure],
    dimensions: &Dimensions,
) -> Result<Measure, EvaluateError> {
    evaluate_in(expression, result, measures, dimensions, &Region::default())?
        .into_measure(result.clone(), dimensions)
        .map_err(EvaluateError::InvalidResult)
}

// Not an evaluation of no cells: a constant spread to a wide result would copy its cell at compile time.
pub fn plan(
    expression: &CheckedExpression,
    result: &MeasureDefinition,
    dimensions: &Dimensions,
) -> Result<(), EvaluateError> {
    plan_node(expression.root(), dimensions)?;
    packing_of(dimensions, result.axes())?;
    Ok(())
}

fn plan_node(node: &CheckedNode, dimensions: &Dimensions) -> Result<(), EvaluateError> {
    for operand in operands(node) {
        plan_node(operand, dimensions)?;
    }
    packing_of(dimensions, &node.axes().into_iter().collect::<Vec<_>>())?;
    Ok(())
}

pub fn edit(
    measures: &[Measure],
    deltas: impl IntoIterator<Item = (MeasureGlobalId, Delta)>,
    formulas: &[(MeasureGlobalId, &CheckedExpression)],
    dimensions: &Dimensions,
) -> Result<Vec<Measure>, EvaluateError> {
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
    Ok(next)
}

fn apply(
    measures: &mut [Measure],
    global_id: MeasureGlobalId,
    delta: Delta,
    changed: &mut HashMap<MeasureGlobalId, Region>,
    dimensions: &Dimensions,
) -> Result<(), EvaluateError> {
    let index = position(measures, global_id)?;
    let axes = measures[index].axes_in_packing_order();
    if let Some(mut region) =
        Region::of_packed(axes, measures[index].packing(), delta.packed_coordinates())
    {
        for &axis in axes {
            if region.members(axis).map(BTreeSet::len)
                == Some(usize_from(required_dimension(dimensions, axis)?.len()))
            {
                region = region.without(axis);
            }
        }
        let region = match changed.remove(&global_id) {
            Some(earlier) => earlier.union(&region),
            None => region,
        };
        changed.insert(global_id, region);
    }
    measures[index] = measures[index]
        .apply(delta)
        .map_err(EvaluateError::InvalidResult)?;
    Ok(())
}

fn position(measures: &[Measure], global_id: MeasureGlobalId) -> Result<usize, EvaluateError> {
    measures
        .iter()
        .position(|measure| measure.global_id() == global_id)
        .ok_or(EvaluateError::MissingMeasure(global_id))
}

fn changed_region(
    expression: &CheckedExpression,
    changed: &HashMap<MeasureGlobalId, Region>,
    dimensions: &Dimensions,
) -> Result<Option<Region>, EvaluateError> {
    changed_in(expression.root(), changed, dimensions)
}

fn recalculate(
    expression: &CheckedExpression,
    result: &Measure,
    region: &Region,
    measures: &[&Measure],
    dimensions: &Dimensions,
) -> Result<Delta, EvaluateError> {
    let definition = result.definition();
    check_packing(result, dimensions)?;
    let new = evaluate_in(expression, definition, measures, dimensions, region)?;
    let old = Columns::from_measure_in(result, region);
    let (packed_coordinates, changes) = new.changes_from(&old);
    result
        .delta_from_packed(packed_coordinates, changes)
        .map_err(EvaluateError::InvalidResult)
}

fn evaluate_in(
    expression: &CheckedExpression,
    result: &MeasureDefinition,
    measures: &[&Measure],
    dimensions: &Dimensions,
    region: &Region,
) -> Result<Columns, EvaluateError> {
    if expression.result_axes() != result.axes() {
        return Err(EvaluateError::ResultAxesChanged(result.global_id()));
    }
    for &axis in result.axes() {
        required_dimension(dimensions, axis)?;
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
            .all(|entry| entry.uses_left == 0 && entry.result.is_none()),
        "every shared result is used up and released"
    );
    Ok(columns
        .broadcast(result.axes(), dimensions)?
        .restrict(region))
}

fn changed_in(
    node: &CheckedNode,
    changed: &HashMap<MeasureGlobalId, Region>,
    dimensions: &Dimensions,
) -> Result<Option<Region>, EvaluateError> {
    Ok(match node {
        CheckedNode::Number(_) => None,
        CheckedNode::Measure { global_id, .. } => changed.get(global_id).cloned(),
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
            filtered_axis,
        } => changed_in(value, changed, dimensions)?.and_then(|mut region| {
            region.constrain(*dimension, members.iter().map(|member| member.0).collect());
            let region = region.non_empty()?;
            Some(match filtered_axis {
                FilteredAxis::Keep => region,
                FilteredAxis::Drop => region.without(*dimension),
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
                                    .filter_map(|&member| attribute.value(MemberIndex(member)))
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

fn check_packing(measure: &Measure, dimensions: &Dimensions) -> Result<(), EvaluateError> {
    if *measure.packing() != packing_of(dimensions, measure.axes_in_packing_order())? {
        return Err(EvaluateError::MeasurePackingChanged(measure.global_id()));
    }
    Ok(())
}

fn find_attribute(
    dimensions: &Dimensions,
    dimension: dimension::DimensionGlobalId,
    attribute: dimension::AttributeGlobalId,
) -> Result<ReferenceAttribute<'_>, EvaluateError> {
    required_dimension(dimensions, dimension)?
        .reference_attributes()
        .find(|candidate| candidate.global_id() == attribute)
        .ok_or(EvaluateError::MissingAttribute(attribute))
}

struct Evaluator<'a, 'e> {
    measures: &'a [&'a Measure],
    dimensions: &'a Dimensions,
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
                filtered_axis,
                ..
            } => {
                let mut region = match filtered_axis {
                    FilteredAxis::Keep => region.clone(),
                    FilteredAxis::Drop => region.clone().without(*dimension),
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
        let columns = self.compute_unchecked(node, region)?;
        debug_assert_eq!(
            columns.axes().iter().copied().collect::<BTreeSet<_>>(),
            node.axes(),
            "plan checks the packing of the axes a node declares"
        );
        Ok(columns)
    }

    fn compute_unchecked(
        &self,
        node: &'e CheckedNode,
        region: &Region,
    ) -> Result<Columns, EvaluateError> {
        match node {
            CheckedNode::Number(value) => Ok(Columns::number(*value)),
            CheckedNode::Measure { global_id, axes } => {
                let measure = self
                    .measures
                    .iter()
                    .find(|measure| measure.global_id() == *global_id)
                    .ok_or(EvaluateError::MissingMeasure(*global_id))?;
                if measure.definition().axes() != *axes {
                    return Err(EvaluateError::MeasureAxesChanged(*global_id));
                }
                check_packing(measure, self.dimensions)?;
                if axes.iter().any(|&axis| region.members(axis).is_some()) {
                    Ok(Columns::from_measure_in(measure, region))
                } else {
                    Ok(Columns::from_measure(measure))
                }
            }
            CheckedNode::Terms(terms) => Columns::add(
                &terms
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
                filtered_axis,
            } => self
                .node(value, &self.operand_region(node, region)?)?
                .filter(*dimension, members, *filtered_axis, self.dimensions),
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
                        let member_count = required_dimension(self.dimensions, dimension)?.len();
                        let members: BTreeSet<u32> = (0..member_count)
                            .filter(|&member| {
                                attribute
                                    .value(MemberIndex(member))
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
