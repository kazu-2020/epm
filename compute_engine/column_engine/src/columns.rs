use std::collections::BTreeSet;
use std::iter;
use std::ops::Range;

use arrow_buffer::ScalarBuffer;
use dimension::{Attribute, Dimension, DimensionUlid, MemberId};
use evaluator::EvaluateError;
use formula::{BinaryOperator, Sign};
use measure::{
    KeyLayout, Measure, MeasureDefinition, MeasureError, first_row, first_unsorted, normalize,
};
use rayon::prelude::*;

use crate::region::Region;

const DENSE_LIMIT: usize = 1 << 22;
// Not one binary search per member above this many: walking the runs is faster.
const SEARCHED_MEMBERS: usize = 64;
// Not one range per thread: a single large range would leave the other threads idle.
const SCANNED_ROWS: usize = 1 << 20;
// Not smaller pieces: below this many rows the cost of handing out work is larger than the work.
const PARALLEL_ROWS: usize = 1 << 14;

#[derive(Clone)]
pub(crate) struct Columns {
    axes: Vec<DimensionUlid>,
    layout: KeyLayout,
    keys: ScalarBuffer<u64>,
    values: ScalarBuffer<f64>,
}

pub(crate) struct ByAxis<'d> {
    pub(crate) output: DimensionUlid,
    pub(crate) source: usize,
    pub(crate) attribute: Option<Attribute<'d>>,
}

enum Source<'c> {
    Cursor,
    Indexed(Index<'c>, Vec<usize>),
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

pub(crate) fn sort_in_key_order<T>(
    items: &mut [T],
    axis: impl Fn(&T) -> DimensionUlid,
    dimensions: &[&Dimension],
) -> Result<(), EvaluateError> {
    for item in items.iter() {
        find_dimension(dimensions, axis(item))?;
    }
    items.sort_by_cached_key(|item| {
        find_dimension(dimensions, axis(item))
            .expect("every axis was found above")
            .key_order()
    });
    Ok(())
}

fn lengths(dimensions: &[&Dimension], axes: &[DimensionUlid]) -> Result<Vec<u32>, EvaluateError> {
    axes.iter()
        .map(|&axis| find_dimension(dimensions, axis).map(Dimension::len))
        .collect()
}

pub(crate) fn layout_of(
    dimensions: &[&Dimension],
    axes: &[DimensionUlid],
) -> Result<KeyLayout, EvaluateError> {
    KeyLayout::new(&lengths(dimensions, axes)?)
        .ok_or(EvaluateError::InvalidResult(MeasureError::KeyTooWide))
}

fn dense_strides(lengths: &[u32]) -> Option<(usize, Vec<usize>)> {
    let mut strides = vec![0; lengths.len()];
    let mut space: usize = 1;
    for (stride, &len) in strides.iter_mut().zip(lengths).rev() {
        *stride = space;
        space = space.checked_mul(len as usize)?;
    }
    (space <= DENSE_LIMIT).then_some((space, strides))
}

fn is_normalized(value: f64) -> bool {
    normalize(value).map(f64::to_bits) == Some(value.to_bits())
}

fn stable_order(keys: &[u64]) -> Vec<usize> {
    let mut rows: Vec<usize> = (0..keys.len()).collect();
    if first_unsorted(keys).is_some() {
        // Not sort_unstable: SUM adds each group in coordinate order.
        rows.par_sort_by_key(|&row| keys[row]);
    }
    rows
}

fn reorder(layout: &KeyLayout, keys: &[u64], positions: &[usize]) -> (KeyLayout, Vec<u64>) {
    let reordered = layout.reordered(positions);
    let keys = keys
        .par_iter()
        .with_min_len(PARALLEL_ROWS)
        .map(|&key| {
            reordered.pack(
                positions
                    .iter()
                    .map(|&position| layout.member(key, position)),
            )
        })
        .collect();
    (reordered, keys)
}

fn pieces(len: usize) -> Vec<Range<usize>> {
    (0..len)
        .step_by(PARALLEL_ROWS)
        .map(|start| start..(start + PARALLEL_ROWS).min(len))
        .collect()
}

fn gather<T: Copy + Send + Sync>(rows: &[usize], from: &[T]) -> Vec<T> {
    rows.par_iter()
        .with_min_len(PARALLEL_ROWS)
        .map(|&row| from[row])
        .collect()
}

fn runs<'r>(keys: &[u64], rows: &'r [usize]) -> Vec<&'r [usize]> {
    rows.chunk_by(|&left, &right| keys[left] == keys[right])
        .collect()
}

fn searched(members: &BTreeSet<u32>) -> bool {
    members.len() <= SEARCHED_MEMBERS
}

struct Index<'c> {
    columns: &'c Columns,
    dense: Option<(Vec<usize>, Vec<u32>)>,
}

impl<'c> Index<'c> {
    fn new(columns: &'c Columns, dimensions: &[&Dimension]) -> Result<Self, EvaluateError> {
        let dense = dense_strides(&lengths(dimensions, &columns.axes)?).map(|(space, strides)| {
            let mut rows = vec![u32::MAX; space];
            for (row, &key) in columns.keys.iter().enumerate() {
                let slot: usize = strides
                    .iter()
                    .zip(columns.layout.members(key))
                    .map(|(stride, member)| stride * member as usize)
                    .sum();
                rows[slot] = row as u32;
            }
            (strides, rows)
        });
        Ok(Index { columns, dense })
    }

    fn find(&self, members: &[u32]) -> Option<usize> {
        match &self.dense {
            Some((strides, rows)) => {
                let slot: usize = strides
                    .iter()
                    .zip(members)
                    .map(|(stride, &member)| stride * member as usize)
                    .sum();
                let row = rows[slot];
                (row != u32::MAX).then_some(row as usize)
            }
            None => self
                .columns
                .keys
                .binary_search(&self.columns.layout.pack(members.iter().copied()))
                .ok(),
        }
    }
}

impl ByAxis<'_> {
    fn member(&self, columns: &Columns, row: usize) -> Option<u32> {
        let member = columns.layout.member(columns.keys[row], self.source);
        match self.attribute {
            None => Some(member),
            Some(attribute) => attribute.value(MemberId(member)).map(|target| target.0),
        }
    }
}

impl Columns {
    fn build(
        axes: Vec<DimensionUlid>,
        members: Vec<Vec<u32>>,
        values: Vec<f64>,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut by_axis: Vec<(DimensionUlid, Vec<u32>)> = axes.into_iter().zip(members).collect();
        sort_in_key_order(&mut by_axis, |&(axis, _)| axis, dimensions)?;
        let (axes, members): (Vec<DimensionUlid>, Vec<Vec<u32>>) = by_axis.into_iter().unzip();
        let layout = layout_of(dimensions, &axes)?;
        let keys: Vec<u64> = (0..values.len())
            .into_par_iter()
            .with_min_len(PARALLEL_ROWS)
            .map(|row| layout.pack(members.iter().map(|column| column[row])))
            .collect();
        Ok(Columns::from_keys(axes, layout, keys, values))
    }

    fn from_keys(
        axes: Vec<DimensionUlid>,
        layout: KeyLayout,
        keys: Vec<u64>,
        values: impl Into<ScalarBuffer<f64>>,
    ) -> Columns {
        let values = values.into();
        if first_unsorted(&keys).is_none()
            && first_row(&values, |&value| !is_normalized(value)).is_none()
        {
            return Columns {
                axes,
                layout,
                keys: keys.into(),
                values,
            };
        }
        let rows: Vec<usize> = stable_order(&keys)
            .into_par_iter()
            .with_min_len(PARALLEL_ROWS)
            .filter(|&row| normalize(values[row]).is_some())
            .collect();
        Columns {
            axes,
            layout,
            keys: gather(&rows, &keys).into(),
            values: rows
                .par_iter()
                .with_min_len(PARALLEL_ROWS)
                .filter_map(|&row| normalize(values[row]))
                .collect::<Vec<f64>>()
                .into(),
        }
    }

    fn normalized(self) -> Columns {
        if first_row(&self.values, |&value| !is_normalized(value)).is_none() {
            return self;
        }
        self.select(&pieces(self.len()), |row| {
            normalize(self.values[row]).is_some()
        })
    }

    fn select(&self, pieces: &[Range<usize>], keep: impl Fn(usize) -> bool + Sync) -> Columns {
        let counts: Vec<usize> = pieces
            .par_iter()
            .map(|piece| piece.clone().filter(|&row| keep(row)).count())
            .collect();
        let total = counts.iter().sum();
        if total == self.len() {
            return self.clone();
        }
        let mut keys = vec![0; total];
        let mut values = vec![0.0; total];
        let mut outputs = Vec::with_capacity(pieces.len());
        let (mut keys_left, mut values_left) = (keys.as_mut_slice(), values.as_mut_slice());
        for &count in &counts {
            let (piece_keys, rest_keys) = keys_left.split_at_mut(count);
            let (piece_values, rest_values) = values_left.split_at_mut(count);
            outputs.push((piece_keys, piece_values));
            (keys_left, values_left) = (rest_keys, rest_values);
        }
        outputs
            .into_par_iter()
            .zip(pieces)
            .for_each(|((piece_keys, piece_values), piece)| {
                let kept = piece.clone().filter(|&row| keep(row));
                for ((key, value), row) in piece_keys.iter_mut().zip(piece_values).zip(kept) {
                    *key = self.keys[row];
                    *value = self.values[row];
                }
            });
        Columns {
            axes: self.axes.clone(),
            layout: self.layout.clone(),
            keys: keys.into(),
            values: values.into(),
        }
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    pub(crate) fn number(value: f64) -> Columns {
        Columns::from_keys(
            Vec::new(),
            KeyLayout::new(&[]).expect("no axes fit in a key"),
            vec![0],
            vec![value],
        )
    }

    pub(crate) fn from_measure(
        measure: &Measure,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        Columns::from_base(&measure.compact(), dimensions)
    }

    pub(crate) fn from_measure_in(
        measure: &Measure,
        region: &Region,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let base = Columns::from_base(measure, dimensions)?.restrict(region);
        let definition = &measure.definition().axes;
        let positions: Vec<usize> = base
            .axes
            .iter()
            .map(|axis| {
                definition
                    .iter()
                    .position(|candidate| candidate == axis)
                    .expect("the columns keep the measure's axes")
            })
            .collect();
        let layout = measure.layout();
        let (keys, changes) = measure.merged_changes();
        let mut inside: Vec<(u64, Option<f64>)> = keys
            .iter()
            .zip(changes)
            .filter(|&(&key, _)| {
                base.axes.iter().zip(&positions).all(|(&axis, &position)| {
                    region
                        .members(axis)
                        .is_none_or(|members| members.contains(&layout.member(key, position)))
                })
            })
            .map(|(&key, change)| (key, change))
            .collect();
        if !positions.is_sorted() {
            for (key, _) in &mut inside {
                *key = base.layout.pack(
                    positions
                        .iter()
                        .map(|&position| layout.member(*key, position)),
                );
            }
            inside.sort_unstable_by_key(|&(key, _)| key);
        }
        Ok(base.with_changes(inside))
    }

    fn with_changes(self, changes: Vec<(u64, Option<f64>)>) -> Columns {
        if changes.is_empty() {
            return self;
        }
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let (mut rows, mut changes) = ((0..self.len()).peekable(), changes.into_iter().peekable());
        loop {
            let (key, value) = match (rows.peek(), changes.peek()) {
                (None, None) => break,
                (Some(_), None) => self.entry(rows.next()),
                (None, Some(_)) => changes.next().expect("peeked above"),
                (Some(&row), Some(&(key, _))) => match self.keys[row].cmp(&key) {
                    std::cmp::Ordering::Less => self.entry(rows.next()),
                    std::cmp::Ordering::Greater => changes.next().expect("peeked above"),
                    std::cmp::Ordering::Equal => {
                        rows.next();
                        changes.next().expect("peeked above")
                    }
                },
            };
            if let Some(value) = value {
                keys.push(key);
                values.push(value);
            }
        }
        Columns {
            axes: self.axes,
            layout: self.layout,
            keys: keys.into(),
            values: values.into(),
        }
    }

    fn entry(&self, row: Option<usize>) -> (u64, Option<f64>) {
        let row = row.expect("peeked above");
        (self.keys[row], Some(self.values[row]))
    }

    fn narrow(
        &self,
        constraints: &[Option<&BTreeSet<u32>>],
        axis: usize,
        rows: Range<usize>,
        kept: &mut Vec<Range<usize>>,
    ) {
        let members = match constraints.get(axis) {
            Some(&Some(members)) if searched(members) => members,
            _ => return kept.push(rows),
        };
        let keys = &self.keys[rows.clone()];
        for &member in members {
            let start = keys.partition_point(|&key| self.layout.member(key, axis) < member);
            let end = keys.partition_point(|&key| self.layout.member(key, axis) <= member);
            self.narrow(
                constraints,
                axis + 1,
                rows.start + start..rows.start + end,
                kept,
            );
        }
    }

    pub(crate) fn restrict(self, region: &Region) -> Columns {
        let constraints: Vec<Option<&BTreeSet<u32>>> =
            self.axes.iter().map(|&axis| region.members(axis)).collect();
        if constraints.iter().all(Option::is_none) {
            return self;
        }
        let mut ranges = Vec::new();
        self.narrow(&constraints, 0, 0..self.len(), &mut ranges);
        // Not one bitmap per axis for a single member: one mask and compare over the whole key tests them all.
        let (mut mask, mut pattern) = (0, 0);
        let mut checks: Vec<(usize, Vec<bool>)> = Vec::new();
        for (axis, constraint) in constraints.iter().enumerate() {
            let Some(members) = *constraint else {
                continue;
            };
            if members.len() == 1
                && let Some(&member) = members.first()
                && self.layout.fits(axis, member)
            {
                mask |= self.layout.field(axis);
                pattern |= self.layout.place(axis, member);
                continue;
            }
            let mut inside = vec![false; members.last().map_or(0, |&last| last as usize + 1)];
            for &member in members {
                inside[member as usize] = true;
            }
            checks.push((axis, inside));
        }
        let pieces: Vec<Range<usize>> = ranges
            .into_iter()
            .flat_map(|range| {
                range
                    .clone()
                    .step_by(SCANNED_ROWS)
                    .map(move |start| start..(start + SCANNED_ROWS).min(range.end))
            })
            .collect();
        // Not select: it reads every key twice, and an edit keeps a few rows out of millions, so the second read is the cost.
        let rows: Vec<usize> = pieces
            .par_iter()
            .flat_map_iter(|piece| {
                self.keys[piece.clone()]
                    .iter()
                    .enumerate()
                    .filter(|&(_, &key)| {
                        key & mask == pattern
                            && checks.iter().all(|(axis, inside)| {
                                inside
                                    .get(self.layout.member(key, *axis) as usize)
                                    .copied()
                                    .unwrap_or(false)
                            })
                    })
                    .map(move |(offset, _)| piece.start + offset)
            })
            .collect();
        if rows.len() == self.len() {
            return self;
        }
        Columns {
            axes: self.axes.clone(),
            layout: self.layout.clone(),
            keys: gather(&rows, &self.keys).into(),
            values: gather(&rows, &self.values).into(),
        }
    }

    pub(crate) fn changes_from(&self, old: &Columns) -> (Vec<u64>, Vec<Option<f64>>) {
        let (mut keys, mut changes) = (Vec::new(), Vec::new());
        let (mut new, mut old_row) = (0, 0);
        while new < self.len() || old_row < old.len() {
            let order = match (new < self.len(), old_row < old.len()) {
                (true, true) => self.keys[new].cmp(&old.keys[old_row]),
                (true, false) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            match order {
                std::cmp::Ordering::Less => {
                    keys.push(self.keys[new]);
                    changes.push(Some(self.values[new]));
                    new += 1;
                }
                std::cmp::Ordering::Greater => {
                    keys.push(old.keys[old_row]);
                    changes.push(None);
                    old_row += 1;
                }
                std::cmp::Ordering::Equal => {
                    if self.values[new].to_bits() != old.values[old_row].to_bits() {
                        keys.push(self.keys[new]);
                        changes.push(Some(self.values[new]));
                    }
                    new += 1;
                    old_row += 1;
                }
            }
        }
        (keys, changes)
    }

    fn from_base(measure: &Measure, dimensions: &[&Dimension]) -> Result<Columns, EvaluateError> {
        let axes = measure.definition().axes.clone();
        let mut order: Vec<usize> = (0..axes.len()).collect();
        sort_in_key_order(&mut order, |&position| axes[position], dimensions)?;
        if order.is_sorted() {
            // Not Columns::from_keys: Measure already rejects unsorted or unnormalized cells.
            return Ok(Columns {
                axes,
                layout: measure.layout().clone(),
                keys: measure.keys().clone(),
                values: measure.values().clone(),
            });
        }
        let (layout, keys) = reorder(measure.layout(), measure.keys(), &order);
        Ok(Columns::from_keys(
            order.iter().map(|&position| axes[position]).collect(),
            layout,
            keys,
            measure.values().clone(),
        ))
    }

    pub(crate) fn layout(&self) -> &KeyLayout {
        &self.layout
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
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut region = Region::default();
        region.constrain(dimension, members.iter().map(|member| member.0).collect());
        let columns = self.restrict(&region);
        if keeps_axis {
            return Ok(columns);
        }
        let position = columns.position(dimension);
        let mut axes = columns.axes.clone();
        axes.remove(position);
        let layout = layout_of(dimensions, &axes)?;
        let keys = columns
            .keys
            .par_iter()
            .with_min_len(PARALLEL_ROWS)
            .map(|&key| {
                layout.pack(
                    columns
                        .layout
                        .members(key)
                        .enumerate()
                        .filter(|&(axis, _)| axis != position)
                        .map(|(_, member)| member),
                )
            })
            .collect();
        Ok(Columns::from_keys(axes, layout, keys, columns.values))
    }

    pub(crate) fn negate(self) -> Columns {
        Columns {
            values: self
                .values
                .par_iter()
                .with_min_len(PARALLEL_ROWS)
                .map(|&value| if value == 0.0 { 0.0 } else { -value })
                .collect::<Vec<f64>>()
                .into(),
            ..self
        }
    }

    pub(crate) fn product(
        operator: BinaryOperator,
        left: &Columns,
        right: &Columns,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let apply: fn(f64, f64) -> f64 = match operator {
            BinaryOperator::Multiply => |left, right| left * right,
            BinaryOperator::Divide => |left, right| left / right,
            BinaryOperator::Add | BinaryOperator::Subtract => {
                unreachable!("+ and - are evaluated as a sum of terms")
            }
        };
        if right.axes.iter().all(|axis| left.axes.contains(axis)) {
            return Columns::lookup_product(left, right, apply, dimensions);
        }
        if left.axes.iter().all(|axis| right.axes.contains(axis)) {
            return Columns::lookup_product(
                right,
                left,
                |right, left| apply(left, right),
                dimensions,
            );
        }
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
        let shared_layout = layout_of(
            dimensions,
            &shared
                .iter()
                .map(|&(position, _)| left.axes[position])
                .collect::<Vec<_>>(),
        )?;
        let project = |columns: &Columns, positions: Vec<usize>| -> Vec<u64> {
            columns
                .keys
                .iter()
                .map(|&key| {
                    shared_layout.pack(
                        positions
                            .iter()
                            .map(|&position| columns.layout.member(key, position)),
                    )
                })
                .collect()
        };
        let left_key = project(left, shared.iter().map(|&(position, _)| position).collect());
        let right_key = project(
            right,
            shared.iter().map(|&(_, position)| position).collect(),
        );
        let left_rows = stable_order(&left_key);
        let right_rows = stable_order(&right_key);
        let mut axes = left.axes.clone();
        axes.extend(extra.iter().map(|&position| right.axes[position]));
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); axes.len()];
        let mut values = Vec::new();
        let mut left_runs = runs(&left_key, &left_rows).into_iter().peekable();
        let mut right_runs = runs(&right_key, &right_rows).into_iter().peekable();
        while let (Some(&left_run), Some(&right_run)) = (left_runs.peek(), right_runs.peek()) {
            match left_key[left_run[0]].cmp(&right_key[right_run[0]]) {
                std::cmp::Ordering::Less => {
                    left_runs.next();
                }
                std::cmp::Ordering::Greater => {
                    right_runs.next();
                }
                std::cmp::Ordering::Equal => {
                    for &left_row in left_run {
                        for &right_row in right_run {
                            let (from_left, from_right) = members.split_at_mut(left.axes.len());
                            for (column, member) in from_left
                                .iter_mut()
                                .zip(left.layout.members(left.keys[left_row]))
                            {
                                column.push(member);
                            }
                            for (column, &position) in from_right.iter_mut().zip(&extra) {
                                column.push(right.layout.member(right.keys[right_row], position));
                            }
                            values.push(apply(left.values[left_row], right.values[right_row]));
                        }
                    }
                    left_runs.next();
                    right_runs.next();
                }
            }
        }
        Columns::build(axes, members, values, dimensions)
    }

    fn lookup_product(
        larger: &Columns,
        smaller: &Columns,
        apply: impl Fn(f64, f64) -> f64 + Sync,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let positions: Vec<usize> = smaller
            .axes
            .iter()
            .map(|&axis| larger.position(axis))
            .collect();
        let index = Index::new(smaller, dimensions)?;
        let values: Vec<f64> = larger
            .keys
            .par_iter()
            .zip(&larger.values[..])
            .with_min_len(PARALLEL_ROWS)
            .map_init(
                || vec![0; positions.len()],
                |key, (&larger_key, &value)| {
                    for (member, &position) in key.iter_mut().zip(&positions) {
                        *member = larger.layout.member(larger_key, position);
                    }
                    index
                        .find(key)
                        .and_then(|other| normalize(apply(value, smaller.values[other])))
                        .unwrap_or(f64::NAN)
                },
            )
            .collect();
        let columns = Columns {
            axes: larger.axes.clone(),
            layout: larger.layout.clone(),
            keys: larger.keys.clone(),
            values: values.into(),
        };
        if !columns.values.par_iter().any(|value| value.is_nan()) {
            return Ok(columns);
        }
        Ok(columns.select(&pieces(columns.len()), |row| !columns.values[row].is_nan()))
    }

    pub(crate) fn add(
        terms: Vec<(Sign, Columns)>,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut axes: Vec<DimensionUlid> = terms
            .iter()
            .flat_map(|(_, term)| term.axes.iter().copied())
            .collect();
        sort_in_key_order(&mut axes, |&axis| axis, dimensions)?;
        axes.dedup();
        let any_full = terms.iter().any(|(_, term)| term.axes.len() == axes.len());
        // Not an empty result when no term is full: ADR 0002 spreads every term, so '人数' + SUM('売上', BY '期間') has a cell on every pair.
        let spread: Vec<Columns> = if any_full {
            Vec::new()
        } else {
            terms
                .iter()
                .map(|(_, term)| term.broadcast(&axes, dimensions))
                .collect::<Result<_, _>>()?
        };
        let creators: Vec<&Columns> = if any_full {
            terms
                .iter()
                .map(|(_, term)| term)
                .filter(|term| term.axes.len() == axes.len())
                .collect()
        } else {
            spread.iter().collect()
        };
        let first = creators[0];
        let keys = creators[1..]
            .iter()
            .fold(first.keys.clone(), |keys, creator| {
                union(keys, &creator.keys)
            });
        let sources = terms
            .iter()
            .map(|(sign, term)| {
                let source = if term.axes.len() == axes.len() {
                    Source::Cursor
                } else {
                    Source::Indexed(
                        Index::new(term, dimensions)?,
                        term.axes.iter().map(|&axis| first.position(axis)).collect(),
                    )
                };
                Ok((*sign, term, source))
            })
            .collect::<Result<Vec<_>, EvaluateError>>()?;
        let mut values = vec![0.0; keys.len()];
        values
            .par_chunks_mut(PARALLEL_ROWS)
            .zip(keys.par_chunks(PARALLEL_ROWS))
            .for_each(|(totals, coordinates)| {
                let mut cursors: Vec<usize> = sources
                    .iter()
                    .map(|(_, term, source)| match source {
                        Source::Cursor => term.keys.partition_point(|&key| key < coordinates[0]),
                        Source::Indexed(..) => 0,
                    })
                    .collect();
                let mut key = Vec::new();
                for (output, &coordinate) in totals.iter_mut().zip(coordinates) {
                    // Not `*output += value`: a read of the zeroed page before the write makes the kernel copy the zero page and flush the TLB of every core.
                    let mut total = 0.0;
                    for ((sign, term, source), next) in sources.iter().zip(&mut cursors) {
                        let value =
                            match source {
                                Source::Cursor => {
                                    if *next < term.len() && term.keys[*next] == coordinate {
                                        *next += 1;
                                        term.values[*next - 1]
                                    } else {
                                        0.0
                                    }
                                }
                                Source::Indexed(index, positions) => {
                                    key.clear();
                                    key.extend(positions.iter().map(|&position| {
                                        first.layout.member(coordinate, position)
                                    }));
                                    index.find(&key).map_or(0.0, |found| term.values[found])
                                }
                            };
                        match sign {
                            Sign::Plus => total += value,
                            Sign::Minus => total -= value,
                        }
                    }
                    *output = total;
                }
            });
        Ok(Columns {
            axes: first.axes.clone(),
            layout: first.layout.clone(),
            keys,
            values: values.into(),
        }
        .normalized())
    }

    pub(crate) fn sum(
        &self,
        by: &[ByAxis<'_>],
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut outputs: Vec<&ByAxis<'_>> = by.iter().collect();
        sort_in_key_order(&mut outputs, |axis| axis.output, dimensions)?;
        let axes: Vec<DimensionUlid> = outputs.iter().map(|axis| axis.output).collect();
        let lengths = lengths(dimensions, &axes)?;
        let layout = layout_of(dimensions, &axes)?;
        if let Some((space, strides)) = dense_strides(&lengths) {
            let slot_of = |row: usize| -> Option<usize> {
                outputs
                    .iter()
                    .zip(&strides)
                    .map(|(axis, stride)| Some(stride * axis.member(self, row)? as usize))
                    .sum()
            };
            let share = space.div_ceil(rayon::current_num_threads()).max(1);
            let owners = space.div_ceil(share);
            let mut totals = vec![0.0; space];
            let mut seen = vec![false; space];
            // Not the handing path with one owner: it would copy every row to the same thread.
            if owners == 1 {
                for row in 0..self.len() {
                    if let Some(slot) = slot_of(row) {
                        totals[slot] += self.values[row];
                        seen[slot] = true;
                    }
                }
            } else {
                // Not partial totals per piece of rows: one thread adds each slot in row order (ADR 0001), so each piece hands its rows to the thread that owns the slot.
                let pieces = pieces(self.len());
                let counts: Vec<Vec<usize>> = pieces
                    .par_iter()
                    .map(|piece| {
                        let mut counts = vec![0; owners];
                        for row in piece.clone() {
                            if let Some(slot) = slot_of(row) {
                                counts[slot / share] += 1;
                            }
                        }
                        counts
                    })
                    .collect();
                // Not a small Vec per piece and owner: glibc keeps their freed holes, and the RSS of 4 threads stayed 0.26 GB above 1 thread.
                let mut handed: Vec<(Vec<u32>, Vec<f64>)> = (0..owners)
                    .map(|owner| {
                        let total = counts.iter().map(|counts| counts[owner]).sum();
                        (vec![0; total], vec![0.0; total])
                    })
                    .collect();
                let mut parts: Vec<Vec<(&mut [u32], &mut [f64])>> =
                    pieces.iter().map(|_| Vec::with_capacity(owners)).collect();
                for (owner, (slots, values)) in handed.iter_mut().enumerate() {
                    let (mut slots, mut values) = (slots.as_mut_slice(), values.as_mut_slice());
                    for (part, counts) in parts.iter_mut().zip(&counts) {
                        let (piece_slots, rest_slots) = slots.split_at_mut(counts[owner]);
                        let (piece_values, rest_values) = values.split_at_mut(counts[owner]);
                        part.push((piece_slots, piece_values));
                        (slots, values) = (rest_slots, rest_values);
                    }
                }
                parts
                    .into_par_iter()
                    .zip(&pieces)
                    .for_each(|(mut part, piece)| {
                        let mut written = vec![0; owners];
                        for row in piece.clone() {
                            if let Some(slot) = slot_of(row) {
                                let owner = slot / share;
                                let (slots, values) = &mut part[owner];
                                slots[written[owner]] = u32::try_from(slot)
                                    .expect("dense_strides keeps space at or below DENSE_LIMIT");
                                values[written[owner]] = self.values[row];
                                written[owner] += 1;
                            }
                        }
                    });
                totals
                    .par_chunks_mut(share)
                    .zip(seen.par_chunks_mut(share))
                    .zip(&handed)
                    .enumerate()
                    .for_each(|(owner, ((totals, seen), (slots, values)))| {
                        for (&slot, &value) in slots.iter().zip(values) {
                            totals[slot as usize - owner * share] += value;
                            seen[slot as usize - owner * share] = true;
                        }
                    });
            }
            let slots: Vec<usize> = (0..space)
                .into_par_iter()
                .with_min_len(PARALLEL_ROWS)
                .filter(|&slot| seen[slot])
                .collect();
            let keys = slots
                .par_iter()
                .with_min_len(PARALLEL_ROWS)
                .map(|&slot| {
                    layout.pack(
                        strides
                            .iter()
                            .zip(&lengths)
                            .map(|(stride, &len)| (slot / stride) as u32 % len),
                    )
                })
                .collect();
            let values = gather(&slots, &totals);
            return Ok(Columns::from_keys(axes, layout, keys, values));
        }
        let (kept, group_keys): (Vec<usize>, Vec<u64>) = (0..self.len())
            .into_par_iter()
            .with_min_len(PARALLEL_ROWS)
            .filter_map(|row| {
                let members: Option<Vec<u32>> =
                    outputs.iter().map(|axis| axis.member(self, row)).collect();
                Some((row, layout.pack(members?)))
            })
            .unzip();
        let order = stable_order(&group_keys);
        let runs = runs(&group_keys, &order);
        let keys = runs.iter().map(|run| group_keys[run[0]]).collect();
        let values: Vec<f64> = runs
            .par_iter()
            .map(|run| {
                run.iter().fold(0.0, |total, &group_row| {
                    total + self.values[kept[group_row]]
                })
            })
            .collect();
        Ok(Columns::from_keys(axes, layout, keys, values))
    }

    pub(crate) fn broadcast(
        &self,
        axes: &[DimensionUlid],
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        if axes.iter().all(|axis| self.axes.contains(axis)) {
            return Ok(Columns {
                axes: self.axes.clone(),
                layout: self.layout.clone(),
                keys: self.keys.clone(),
                values: self.values.clone(),
            });
        }
        let mut spread_axes = self.axes.clone();
        let mut members: Vec<Vec<u32>> = (0..self.axes.len())
            .map(|axis| {
                self.keys
                    .iter()
                    .map(|&key| self.layout.member(key, axis))
                    .collect()
            })
            .collect();
        let mut values = self.values.to_vec();
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
        Columns::build(spread_axes, members, values, dimensions)
    }

    pub(crate) fn into_measure(
        self,
        definition: MeasureDefinition,
        dimensions: &[&Dimension],
    ) -> Result<Measure, MeasureError> {
        let positions: Vec<usize> = definition
            .axes
            .iter()
            .map(|&axis| self.position(axis))
            .collect();
        if positions.is_sorted() {
            return Measure::try_from_keys(definition, self.keys, self.values, dimensions);
        }
        let (_, keys) = reorder(&self.layout, &self.keys, &positions);
        let rows = stable_order(&keys);
        Measure::try_from_keys(
            definition,
            rows.iter().map(|&row| keys[row]).collect(),
            rows.iter().map(|&row| self.values[row]).collect(),
            dimensions,
        )
    }
}

fn union(mine: ScalarBuffer<u64>, theirs: &ScalarBuffer<u64>) -> ScalarBuffer<u64> {
    if mine.ptr_eq(theirs) {
        return mine;
    }
    let mut keys = Vec::with_capacity(mine.len() + theirs.len());
    let (mut own, mut their) = (0, 0);
    while own < mine.len() || their < theirs.len() {
        let next = match (mine.get(own), theirs.get(their)) {
            (Some(&left), Some(&right)) if left == right => {
                own += 1;
                their += 1;
                left
            }
            (Some(&left), Some(&right)) if left < right => {
                own += 1;
                left
            }
            (Some(&left), None) => {
                own += 1;
                left
            }
            (_, Some(&right)) => {
                their += 1;
                right
            }
            (None, None) => unreachable!("the loop stops when both are done"),
        };
        keys.push(next);
    }
    keys.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_keys_still_drop_values_that_are_not_normalized() {
        let axis = DimensionUlid(ulid::Ulid(1));
        let layout = KeyLayout::new(&[8]).expect("3 bits fit in a key");

        let columns = Columns::from_keys(vec![axis], layout, vec![0, 1], vec![f64::NAN, -0.0]);

        assert_eq!(&columns.keys[..], &[1]);
        assert_eq!(columns.values[0].to_bits(), 0);
    }

    #[test]
    fn adding_terms_on_the_same_coordinates_keeps_their_keys() {
        let fixture = datagen::cases::Fixture::default();
        let dimensions: Vec<&Dimension> = fixture.dimensions.iter().collect();
        let axis = dimensions[1].ulid();
        let layout = KeyLayout::new(&[3]).expect("2 bits fit in a key");
        let sales = Columns::from_keys(
            vec![axis],
            layout.clone(),
            vec![0, 1, 2],
            vec![3.0, 4.0, 5.0],
        );
        let cost = Columns {
            axes: sales.axes.clone(),
            layout: sales.layout.clone(),
            keys: sales.keys.clone(),
            values: vec![1.0, 1.0, 1.0].into(),
        };
        let keys = sales.keys.clone();

        let profit = Columns::add(vec![(Sign::Plus, sales), (Sign::Minus, cost)], &dimensions)
            .expect("both terms have every axis");

        assert!(profit.keys.ptr_eq(&keys));
        assert_eq!(&profit.values[..], &[2.0, 3.0, 4.0]);
    }
}
