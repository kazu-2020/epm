use std::collections::{HashMap, HashSet};

use dimension::{Attribute, Dimension, DimensionUlid, MemberId};
use formula::{ProductOperator, Sign};
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

enum Source {
    Kept(usize),
    Repeated(u32),
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

    pub(crate) fn product(operator: ProductOperator, left: Cells, right: Cells) -> Cells {
        let mut axes = left.axes.clone();
        axes.extend(right.axes.iter().filter(|axis| !left.axes.contains(axis)));
        let apply: fn(f64, f64) -> f64 = match operator {
            ProductOperator::Multiply => |left, right| left * right,
            ProductOperator::Divide => |left, right| left / right,
        };
        // Not broadcast: only cells present on both sides survive, so spreading a sparse side over every member would build cells the join discards.
        Cells {
            axes,
            values: normalized(join(&left, &right, apply)),
        }
    }

    pub(crate) fn add(
        terms: Vec<(Sign, Cells)>,
        dimensions: &[&Dimension],
    ) -> Result<Cells, EvaluateError> {
        let mut axes: Vec<DimensionUlid> = Vec::new();
        for (_, term) in &terms {
            for &axis in &term.axes {
                if !axes.contains(&axis) {
                    axes.push(axis);
                }
            }
        }
        let any_full = terms.iter().any(|(_, term)| term.axes.len() == axes.len());
        // Not only the full terms when none is full: spreading every term keeps '人数' + SUM('売上', BY '期間') defined on every pair.
        let creates_cells = |term: &Cells| !any_full || term.axes.len() == axes.len();
        let mut coordinates: HashSet<Vec<MemberId>> = HashSet::new();
        for (_, term) in &terms {
            if creates_cells(term) {
                coordinates.extend(term.broadcast(&axes, dimensions)?.values.into_keys());
            }
        }
        let positions: Vec<Vec<usize>> = terms
            .iter()
            .map(|(_, term)| {
                term.axes
                    .iter()
                    .map(|axis| {
                        axes.iter()
                            .position(|own| own == axis)
                            .expect("axes has every term's axes")
                    })
                    .collect()
            })
            .collect();
        let values = normalized(coordinates.into_iter().map(|coordinate| {
            let mut total = 0.0;
            for ((sign, term), positions) in terms.iter().zip(&positions) {
                let key: Vec<MemberId> = positions
                    .iter()
                    .map(|&position| coordinate[position])
                    .collect();
                let value = term.values.get(&key).copied().unwrap_or(0.0);
                match sign {
                    Sign::Plus => total += value,
                    Sign::Minus => total -= value,
                }
            }
            (coordinate, total)
        }));
        Ok(Cells { axes, values })
    }

    pub(crate) fn filter(
        mut self,
        dimension: DimensionUlid,
        members: &[MemberId],
        keeps_axis: bool,
    ) -> Cells {
        let position = self
            .axes
            .iter()
            .position(|&axis| axis == dimension)
            .expect("a checked expression filters an axis of its value");
        self.values
            .retain(|coordinate, _| members.contains(&coordinate[position]));
        if keeps_axis {
            return self;
        }
        self.axes.remove(position);
        Cells {
            axes: self.axes,
            values: self
                .values
                .into_iter()
                .map(|(mut coordinate, value)| {
                    coordinate.remove(position);
                    (coordinate, value)
                })
                .collect(),
        }
    }

    pub(crate) fn sum(
        &self,
        by: &[ByAxis<'_>],
        dimensions: &[&Dimension],
    ) -> Result<Cells, EvaluateError> {
        let orders = self
            .axes
            .iter()
            .map(|&axis| Ok(find_dimension(dimensions, axis)?.key_order()))
            .collect::<Result<Vec<_>, EvaluateError>>()?;
        let mut order: Vec<usize> = (0..self.axes.len()).collect();
        order.sort_by_key(|&axis| orders[axis]);
        let mut cells: Vec<(&Vec<MemberId>, &f64)> = self.values.iter().collect();
        // Not the map's order: f64 addition depends on order, and the map's changes from run to run.
        cells.sort_unstable_by(|(left, _), (right, _)| {
            order
                .iter()
                .map(|&axis| left[axis])
                .cmp(order.iter().map(|&axis| right[axis]))
        });
        let mut groups: HashMap<Vec<MemberId>, f64> = HashMap::new();
        for (coordinate, &value) in cells {
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
        Ok(Cells {
            axes: by.iter().map(|axis| axis.output).collect(),
            values: normalized(groups),
        })
    }

    pub(crate) fn broadcast(
        &self,
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
        for (coordinate, &value) in &self.values {
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
