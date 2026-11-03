use std::cmp::Ordering;
use std::iter;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use arrow_array::{ArrayRef, Float64Array, RecordBatch, UInt32Array};
use dimension::{Attribute, Dimension, DimensionUlid, MemberId};
use evaluator::EvaluateError;
use formula::{BinaryOperator, Sign};
use measure::{Measure, MeasureDefinition, MeasureError, normalize};

pub(crate) struct Columns {
    axes: Vec<DimensionUlid>,
    members: Vec<Vec<u32>>,
    values: Vec<f64>,
}

pub(crate) struct ByAxis<'d> {
    pub(crate) output: DimensionUlid,
    pub(crate) source: usize,
    pub(crate) attribute: Option<Attribute<'d>>,
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

fn compare(left: &[&[u32]], left_row: usize, right: &[&[u32]], right_row: usize) -> Ordering {
    left.iter()
        .map(|column| column[left_row])
        .cmp(right.iter().map(|column| column[right_row]))
}

fn sorted_rows(columns: &[&[u32]], len: usize) -> Vec<usize> {
    let mut rows: Vec<usize> = (0..len).collect();
    rows.sort_by(|&left, &right| compare(columns, left, columns, right));
    rows
}

fn runs<'r>(columns: &[&[u32]], rows: &'r [usize]) -> Vec<&'r [usize]> {
    rows.chunk_by(|&left, &right| compare(columns, left, columns, right) == Ordering::Equal)
        .collect()
}

impl ByAxis<'_> {
    fn member(&self, columns: &[Vec<u32>], row: usize) -> Option<u32> {
        let member = columns[self.source][row];
        match self.attribute {
            None => Some(member),
            Some(attribute) => attribute.value(MemberId(member)).map(|target| target.0),
        }
    }
}

impl Columns {
    fn new(axes: Vec<DimensionUlid>, members: Vec<Vec<u32>>, values: Vec<f64>) -> Columns {
        let mut by_axis: Vec<(DimensionUlid, Vec<u32>)> = axes.into_iter().zip(members).collect();
        by_axis.sort_by_key(|&(axis, _)| axis);
        let (axes, members): (Vec<DimensionUlid>, Vec<Vec<u32>>) = by_axis.into_iter().unzip();
        let columns: Vec<&[u32]> = members.iter().map(Vec::as_slice).collect();
        let mut rows: Vec<usize> = (0..values.len())
            .filter(|&row| normalize(values[row]).is_some())
            .collect();
        rows.sort_unstable_by(|&left, &right| compare(&columns, left, &columns, right));
        Columns {
            axes,
            members: columns
                .iter()
                .map(|column| rows.iter().map(|&row| column[row]).collect())
                .collect(),
            values: rows
                .iter()
                .filter_map(|&row| normalize(values[row]))
                .collect(),
        }
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn columns(&self) -> Vec<&[u32]> {
        self.members.iter().map(Vec::as_slice).collect()
    }

    pub(crate) fn number(value: f64) -> Columns {
        Columns::new(Vec::new(), Vec::new(), vec![value])
    }

    pub(crate) fn from_measure(measure: &Measure) -> Columns {
        let measure = measure.compact();
        let axes = measure.definition().axes.clone();
        let base = measure.base();
        let members = (0..axes.len())
            .map(|axis| {
                base.column(axis)
                    .as_primitive::<UInt32Type>()
                    .values()
                    .to_vec()
            })
            .collect();
        let values = base
            .column(axes.len())
            .as_primitive::<Float64Type>()
            .values()
            .to_vec();
        Columns::new(axes, members, values)
    }

    pub(crate) fn position(&self, dimension: DimensionUlid) -> usize {
        self.axes
            .iter()
            .position(|&axis| axis == dimension)
            .expect("the columns have every axis asked for")
    }

    pub(crate) fn filter(
        self,
        dimension: DimensionUlid,
        members: &[MemberId],
        keeps_axis: bool,
    ) -> Columns {
        let position = self.position(dimension);
        let kept: Vec<usize> = (0..self.len())
            .filter(|&row| members.contains(&MemberId(self.members[position][row])))
            .collect();
        let mut axes = self.axes;
        let mut columns = self.members;
        if !keeps_axis {
            axes.remove(position);
            columns.remove(position);
        }
        Columns {
            axes,
            members: columns
                .iter()
                .map(|column| kept.iter().map(|&row| column[row]).collect())
                .collect(),
            values: kept.iter().map(|&row| self.values[row]).collect(),
        }
    }

    pub(crate) fn negate(self) -> Columns {
        let values = self.values.iter().map(|value| -value).collect();
        Columns::new(self.axes, self.members, values)
    }

    pub(crate) fn product(operator: BinaryOperator, left: &Columns, right: &Columns) -> Columns {
        let apply: fn(f64, f64) -> f64 = match operator {
            BinaryOperator::Multiply => |left, right| left * right,
            BinaryOperator::Divide => |left, right| left / right,
            BinaryOperator::Add | BinaryOperator::Subtract => {
                unreachable!("+ and - are evaluated as a sum of terms")
            }
        };
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
        let left_key: Vec<&[u32]> = shared
            .iter()
            .map(|&(position, _)| left.members[position].as_slice())
            .collect();
        let right_key: Vec<&[u32]> = shared
            .iter()
            .map(|&(_, position)| right.members[position].as_slice())
            .collect();
        let left_rows = sorted_rows(&left_key, left.len());
        let right_rows = sorted_rows(&right_key, right.len());
        let mut axes = left.axes.clone();
        axes.extend(extra.iter().map(|&position| right.axes[position]));
        let mut members = vec![Vec::new(); axes.len()];
        let mut values = Vec::new();
        let mut left_runs = runs(&left_key, &left_rows).into_iter().peekable();
        let mut right_runs = runs(&right_key, &right_rows).into_iter().peekable();
        while let (Some(&left_run), Some(&right_run)) = (left_runs.peek(), right_runs.peek()) {
            match compare(&left_key, left_run[0], &right_key, right_run[0]) {
                Ordering::Less => {
                    left_runs.next();
                }
                Ordering::Greater => {
                    right_runs.next();
                }
                Ordering::Equal => {
                    for &left_row in left_run {
                        for &right_row in right_run {
                            let (from_left, from_right) = members.split_at_mut(left.axes.len());
                            for (column, source) in from_left.iter_mut().zip(&left.members) {
                                column.push(source[left_row]);
                            }
                            for (column, &position) in from_right.iter_mut().zip(&extra) {
                                column.push(right.members[position][right_row]);
                            }
                            values.push(apply(left.values[left_row], right.values[right_row]));
                        }
                    }
                    left_runs.next();
                    right_runs.next();
                }
            }
        }
        Columns::new(axes, members, values)
    }

    pub(crate) fn add(
        terms: Vec<(Sign, Columns)>,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut axes: Vec<DimensionUlid> = terms
            .iter()
            .flat_map(|(_, term)| term.axes.iter().copied())
            .collect();
        axes.sort_unstable();
        axes.dedup();
        let any_full = terms.iter().any(|(_, term)| term.axes.len() == axes.len());
        // Not only the full terms when none is full: spreading every term keeps '人数' + SUM('売上', BY '期間') defined on every pair.
        let creates_cells = |term: &Columns| !any_full || term.axes.len() == axes.len();
        let mut members = vec![Vec::new(); axes.len()];
        let mut len = 0;
        for (_, term) in &terms {
            if creates_cells(term) {
                let spread = term.broadcast(&axes, dimensions)?;
                len += spread.len();
                for (column, source) in members.iter_mut().zip(spread.members) {
                    column.extend(source);
                }
            }
        }
        let coordinates = Columns::new(axes, members, vec![0.0; len]).unique();
        let positions: Vec<Vec<usize>> = terms
            .iter()
            .map(|(_, term)| {
                term.axes
                    .iter()
                    .map(|&axis| coordinates.position(axis))
                    .collect()
            })
            .collect();
        let mut key = Vec::new();
        let values = (0..coordinates.len())
            .map(|row| {
                let mut total = 0.0;
                for ((sign, term), positions) in terms.iter().zip(&positions) {
                    key.clear();
                    key.extend(
                        positions
                            .iter()
                            .map(|&position| coordinates.members[position][row]),
                    );
                    let value = term.value_at(&key).unwrap_or(0.0);
                    match sign {
                        Sign::Plus => total += value,
                        Sign::Minus => total -= value,
                    }
                }
                total
            })
            .collect();
        Ok(Columns::new(coordinates.axes, coordinates.members, values))
    }

    fn unique(self) -> Columns {
        let columns = self.columns();
        let kept: Vec<usize> = (0..self.len())
            .filter(|&row| row == 0 || compare(&columns, row - 1, &columns, row) != Ordering::Equal)
            .collect();
        Columns {
            members: columns
                .iter()
                .map(|column| kept.iter().map(|&row| column[row]).collect())
                .collect(),
            values: kept.iter().map(|&row| self.values[row]).collect(),
            axes: self.axes,
        }
    }

    fn value_at(&self, coordinate: &[u32]) -> Option<f64> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self
                .members
                .iter()
                .map(|column| column[middle])
                .cmp(coordinate.iter().copied())
            {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(self.values[middle]),
            }
        }
        None
    }

    pub(crate) fn sum(&self, by: &[ByAxis<'_>]) -> Columns {
        let kept: Vec<usize> = (0..self.len())
            .filter(|&row| {
                by.iter()
                    .all(|axis| axis.member(&self.members, row).is_some())
            })
            .collect();
        let mut groups: Vec<(DimensionUlid, Vec<u32>)> = by
            .iter()
            .map(|axis| {
                let members = kept
                    .iter()
                    .map(|&row| {
                        axis.member(&self.members, row)
                            .expect("kept rows have a member on every BY axis")
                    })
                    .collect();
                (axis.output, members)
            })
            .collect();
        groups.sort_by_key(|&(axis, _)| axis);
        let group_columns: Vec<&[u32]> =
            groups.iter().map(|(_, column)| column.as_slice()).collect();
        // Stable, so each group adds its values in the value's coordinate order.
        let order = sorted_rows(&group_columns, kept.len());
        let runs = runs(&group_columns, &order);
        let members = group_columns
            .iter()
            .map(|column| runs.iter().map(|run| column[run[0]]).collect())
            .collect();
        let values = runs
            .iter()
            .map(|run| {
                run.iter().fold(0.0, |total, &group_row| {
                    total + self.values[kept[group_row]]
                })
            })
            .collect();
        Columns::new(
            groups.into_iter().map(|(axis, _)| axis).collect(),
            members,
            values,
        )
    }

    pub(crate) fn broadcast(
        &self,
        axes: &[DimensionUlid],
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut spread_axes = self.axes.clone();
        let mut members = self.members.clone();
        let mut values = self.values.clone();
        for &axis in axes {
            if spread_axes.contains(&axis) {
                continue;
            }
            let len = find_dimension(dimensions, axis)?.len();
            let copies = len as usize;
            for column in &mut members {
                *column = column
                    .iter()
                    .flat_map(|&member| iter::repeat_n(member, copies))
                    .collect();
            }
            members.push((0..values.len()).flat_map(|_| 0..len).collect());
            values = values
                .iter()
                .flat_map(|&value| iter::repeat_n(value, copies))
                .collect();
            spread_axes.push(axis);
        }
        Ok(Columns::new(spread_axes, members, values))
    }

    pub(crate) fn into_measure(
        self,
        definition: MeasureDefinition,
        dimensions: &[&Dimension],
    ) -> Result<Measure, MeasureError> {
        let columns: Vec<&[u32]> = definition
            .axes
            .iter()
            .map(|&axis| self.members[self.position(axis)].as_slice())
            .collect();
        let rows = sorted_rows(&columns, self.len());
        let mut arrays: Vec<ArrayRef> = columns
            .iter()
            .map(|column| {
                Arc::new(UInt32Array::from_iter_values(
                    rows.iter().map(|&row| column[row]),
                )) as ArrayRef
            })
            .collect();
        arrays.push(Arc::new(Float64Array::from_iter_values(
            rows.iter().map(|&row| self.values[row]),
        )));
        let base = RecordBatch::try_new(Arc::new(definition.schema()), arrays)
            .expect("the columns follow the result's schema");
        Measure::try_new(definition, base, dimensions)
    }
}
