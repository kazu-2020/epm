use std::collections::{HashMap, HashSet};

use dimension::{Attribute, Dimension, DimensionUlid, MemberId};
use formula::BinaryOperator;
use measure::{Measure, MeasureDefinition, MeasureError, normalize};

use crate::evaluate_error::EvaluateError;

pub(crate) struct Cells {
    pub(crate) axes: Vec<DimensionUlid>,
    values: HashMap<Vec<MemberId>, f64>,
}

pub(crate) struct ByAxis<'d> {
    pub(crate) output: DimensionUlid,
    pub(crate) source: usize,
    pub(crate) attribute: Option<Attribute<'d>>,
}

#[derive(Clone, Copy)]
enum Presence {
    Either,
    Both,
}

enum Source {
    Kept(usize),
    Repeated(u32),
}

fn rule(operator: BinaryOperator) -> (Presence, fn(f64, f64) -> f64) {
    match operator {
        BinaryOperator::Add => (Presence::Either, |left, right| left + right),
        BinaryOperator::Subtract => (Presence::Either, |left, right| left - right),
        BinaryOperator::Multiply => (Presence::Both, |left, right| left * right),
        BinaryOperator::Divide => (Presence::Both, |left, right| left / right),
    }
}

pub(crate) fn find_dimension<'d>(
    dimensions: &[&'d Dimension],
    ulid: DimensionUlid,
) -> Result<&'d Dimension, EvaluateError> {
    dimensions
        .iter()
        .find(|dimension| dimension.ulid() == ulid)
        .copied()
        .ok_or(EvaluateError::MissingDimension(ulid))
}

fn normalized(
    values: impl IntoIterator<Item = (Vec<MemberId>, f64)>,
) -> HashMap<Vec<MemberId>, f64> {
    values
        .into_iter()
        .filter_map(|(coordinate, value)| Some((coordinate, normalize(value)?)))
        .collect()
}

fn join(left: &Cells, right: &Cells, apply: fn(f64, f64) -> f64) -> Vec<(Vec<MemberId>, f64)> {
    let shared: Vec<(usize, usize)> = left
        .axes
        .iter()
        .enumerate()
        .filter_map(|(left_position, axis)| {
            let right_position = right.axes.iter().position(|own| own == axis)?;
            Some((left_position, right_position))
        })
        .collect();
    let extra: Vec<usize> = (0..right.axes.len())
        .filter(|&position| !left.axes.contains(&right.axes[position]))
        .collect();
    let mut by_shared: HashMap<Vec<MemberId>, Vec<(&Vec<MemberId>, f64)>> = HashMap::new();
    for (coordinate, &value) in &right.values {
        let key = shared
            .iter()
            .map(|&(_, position)| coordinate[position])
            .collect();
        by_shared.entry(key).or_default().push((coordinate, value));
    }
    let mut joined = Vec::new();
    for (left_coordinate, &left_value) in &left.values {
        let key: Vec<MemberId> = shared
            .iter()
            .map(|&(position, _)| left_coordinate[position])
            .collect();
        for &(right_coordinate, right_value) in by_shared.get(&key).into_iter().flatten() {
            let mut coordinate = left_coordinate.clone();
            coordinate.extend(extra.iter().map(|&position| right_coordinate[position]));
            joined.push((coordinate, apply(left_value, right_value)));
        }
    }
    joined
}

impl Cells {
    pub(crate) fn number(value: f64) -> Cells {
        Cells {
            axes: Vec::new(),
            values: normalized([(Vec::new(), value)]),
        }
    }

    pub(crate) fn from_measure(measure: &Measure) -> Cells {
        Cells {
            axes: measure.definition().axes.clone(),
            values: measure.cells().collect(),
        }
    }

    pub(crate) fn negate(self) -> Cells {
        Cells {
            axes: self.axes,
            values: normalized(
                self.values
                    .into_iter()
                    .map(|(coordinate, value)| (coordinate, -value)),
            ),
        }
    }

    pub(crate) fn combine(
        operator: BinaryOperator,
        left: Cells,
        right: Cells,
        dimensions: &[&Dimension],
    ) -> Result<Cells, EvaluateError> {
        let mut axes = left.axes.clone();
        axes.extend(right.axes.iter().filter(|axis| !left.axes.contains(axis)));
        let (presence, apply) = rule(operator);
        let values = match presence {
            Presence::Either => {
                let left = left.broadcast(&axes, dimensions)?;
                let right = right.broadcast(&axes, dimensions)?;
                let coordinates: HashSet<&Vec<MemberId>> =
                    left.values.keys().chain(right.values.keys()).collect();
                normalized(coordinates.into_iter().map(|coordinate| {
                    let side = |cells: &Cells| cells.values.get(coordinate).copied().unwrap_or(0.0);
                    (coordinate.clone(), apply(side(&left), side(&right)))
                }))
            }
            // Not broadcast like Either: only cells present on both sides survive, so spreading a sparse side over every member would build cells the join discards.
            Presence::Both => normalized(join(&left, &right, apply)),
        };
        Ok(Cells { axes, values })
    }

    pub(crate) fn sum(&self, by: &[ByAxis<'_>]) -> Cells {
        let mut order: Vec<usize> = (0..self.axes.len()).collect();
        order.sort_by_key(|&axis| self.axes[axis]);
        let mut cells: Vec<(&Vec<MemberId>, f64)> = self
            .values
            .iter()
            .map(|(coordinate, &value)| (coordinate, value))
            .collect();
        // Not the map's order: f64 addition depends on order, and the map's changes from run to run.
        cells.sort_unstable_by(|(left, _), (right, _)| {
            order
                .iter()
                .map(|&axis| left[axis])
                .cmp(order.iter().map(|&axis| right[axis]))
        });
        let mut groups: HashMap<Vec<MemberId>, f64> = HashMap::new();
        for (coordinate, value) in cells {
            // Not a group of its own: the design document drops a member whose attribute is null from the sum.
            let group: Option<Vec<MemberId>> = by
                .iter()
                .map(|axis| {
                    let member = coordinate[axis.source];
                    match axis.attribute {
                        None => Some(member),
                        Some(attribute) => attribute.value(member),
                    }
                })
                .collect();
            if let Some(group) = group {
                *groups.entry(group).or_insert(0.0) += value;
            }
        }
        Cells {
            axes: by.iter().map(|axis| axis.output).collect(),
            values: normalized(groups),
        }
    }

    pub(crate) fn broadcast(
        self,
        axes: &[DimensionUlid],
        dimensions: &[&Dimension],
    ) -> Result<Cells, EvaluateError> {
        let sources = axes
            .iter()
            .map(
                |&axis| match self.axes.iter().position(|&own| own == axis) {
                    Some(position) => Ok(Source::Kept(position)),
                    None => find_dimension(dimensions, axis)
                        .map(|dimension| Source::Repeated(dimension.len())),
                },
            )
            .collect::<Result<Vec<_>, _>>()?;
        let mut values = HashMap::new();
        for (coordinate, value) in self.values {
            let mut coordinates = vec![Vec::with_capacity(axes.len())];
            for source in &sources {
                let members: Vec<MemberId> = match *source {
                    Source::Kept(position) => vec![coordinate[position]],
                    Source::Repeated(len) => (0..len).map(MemberId).collect(),
                };
                coordinates = coordinates
                    .into_iter()
                    .flat_map(|prefix| {
                        members.iter().map(move |&member| {
                            let mut extended = prefix.clone();
                            extended.push(member);
                            extended
                        })
                    })
                    .collect();
            }
            values.extend(coordinates.into_iter().map(|extended| (extended, value)));
        }
        Ok(Cells {
            axes: axes.to_vec(),
            values,
        })
    }

    pub(crate) fn into_measure(
        self,
        definition: MeasureDefinition,
        dimensions: &[&Dimension],
    ) -> Result<Measure, MeasureError> {
        Measure::from_cells(definition, self.values.into_iter().collect(), dimensions)
    }
}
