use std::collections::{BTreeMap, BTreeSet};
use std::iter;
use std::ops::Range;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use arrow_array::{ArrayRef, Float64Array, RecordBatch, UInt32Array};
use arrow_buffer::ScalarBuffer;
use dimension::{Attribute, Dimension, DimensionUlid, MemberId};
use evaluator::EvaluateError;
use formula::{BinaryOperator, Sign};
use measure::{KeyLayout, Measure, MeasureDefinition, MeasureError, normalize};

use crate::region::Region;

const DENSE_LIMIT: usize = 1 << 22;
// Not one binary search per member above this many: walking the runs is faster.
const SEARCHED_MEMBERS: usize = 64;
// Not one range per thread: a single large range would leave the other threads idle.
const SCANNED_ROWS: usize = 1 << 20;

type Column = ScalarBuffer<u32>;

pub(crate) struct Columns {
    axes: Vec<DimensionUlid>,
    layout: KeyLayout,
    keys: ScalarBuffer<u64>,
    values: ScalarBuffer<f64>,
    // Not always unpacked: a result on the rows of a stored measure keeps that measure's columns instead of a copy.
    members: Option<Vec<Column>>,
}

pub(crate) struct ByAxis<'d> {
    pub(crate) output: DimensionUlid,
    pub(crate) source: usize,
    pub(crate) attribute: Option<Attribute<'d>>,
}

enum Source<'c> {
    Cursor(usize),
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

fn lengths(dimensions: &[&Dimension], axes: &[DimensionUlid]) -> Result<Vec<u32>, EvaluateError> {
    axes.iter()
        .map(|&axis| find_dimension(dimensions, axis).map(Dimension::len))
        .collect()
}

fn layout_of(
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
    if !keys.is_sorted() {
        // Not sort_unstable: SUM adds each group in coordinate order.
        rows.sort_by_key(|&row| keys[row]);
    }
    rows
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
        by_axis.sort_by_key(|&(axis, _)| axis);
        let (axes, members): (Vec<DimensionUlid>, Vec<Vec<u32>>) = by_axis.into_iter().unzip();
        let layout = layout_of(dimensions, &axes)?;
        let keys: Vec<u64> = (0..values.len())
            .map(|row| layout.pack(members.iter().map(|column| column[row])))
            .collect();
        let in_order = keys.is_sorted() && values.iter().all(|&value| is_normalized(value));
        Ok(Columns {
            members: in_order.then(|| members.into_iter().map(Column::from).collect()),
            ..Columns::from_keys(axes, layout, keys, values)
        })
    }

    fn from_keys(
        axes: Vec<DimensionUlid>,
        layout: KeyLayout,
        keys: Vec<u64>,
        values: Vec<f64>,
    ) -> Columns {
        let rows: Vec<usize> = stable_order(&keys)
            .into_iter()
            .filter(|&row| normalize(values[row]).is_some())
            .collect();
        if rows.len() == keys.len()
            && rows.iter().enumerate().all(|(at, &row)| at == row)
            && values.iter().all(|&value| is_normalized(value))
        {
            return Columns {
                axes,
                layout,
                keys: keys.into(),
                values: values.into(),
                members: None,
            };
        }
        Columns {
            axes,
            layout,
            keys: rows.iter().map(|&row| keys[row]).collect(),
            values: rows
                .iter()
                .filter_map(|&row| normalize(values[row]))
                .collect(),
            members: None,
        }
    }

    fn normalized(self) -> Columns {
        if self.values.iter().all(|&value| is_normalized(value)) {
            return self;
        }
        let rows: Vec<usize> = (0..self.len())
            .filter(|&row| normalize(self.values[row]).is_some())
            .collect();
        self.take(&rows)
    }

    fn take(&self, rows: &[usize]) -> Columns {
        Columns {
            axes: self.axes.clone(),
            layout: self.layout.clone(),
            keys: rows.iter().map(|&row| self.keys[row]).collect(),
            values: rows
                .iter()
                .map(|&row| normalize(self.values[row]).unwrap_or(0.0))
                .collect(),
            members: None,
        }
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn number(value: f64) -> Columns {
        Columns::from_keys(
            Vec::new(),
            KeyLayout::new(&[]).expect("no axes fit in a key"),
            vec![0],
            vec![value],
        )
    }

    pub(crate) fn from_measure(measure: &Measure) -> Columns {
        Columns::from_base(&measure.compact())
    }

    pub(crate) fn from_measure_in(measure: &Measure, region: &Region) -> Columns {
        let base = Columns::from_base(measure).restrict(region);
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
        let mut changes = BTreeMap::new();
        for (coordinate, value) in measure.changes() {
            let key: Vec<u32> = positions
                .iter()
                .map(|&position| coordinate[position].0)
                .collect();
            let inside = base.axes.iter().zip(&key).all(|(&axis, member)| {
                region
                    .members(axis)
                    .is_none_or(|members| members.contains(member))
            });
            if inside {
                changes.insert(base.layout.pack(key), value);
            }
        }
        base.with_changes(changes)
    }

    fn with_changes(self, changes: BTreeMap<u64, Option<f64>>) -> Columns {
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
            members: None,
        }
    }

    fn entry(&self, row: Option<usize>) -> (u64, Option<f64>) {
        let row = row.expect("peeked above");
        (self.keys[row], Some(self.values[row]))
    }

    fn cell(&self, row: Option<usize>) -> (Vec<u32>, Option<f64>) {
        let row = row.expect("peeked above");
        (
            self.layout.members(self.keys[row]).collect(),
            Some(self.values[row]),
        )
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
        let checks: Vec<(usize, Vec<bool>)> = constraints
            .iter()
            .enumerate()
            .filter_map(|(axis, constraint)| {
                let members = (*constraint)?;
                let mut inside = vec![false; members.last().map_or(0, |&last| last as usize + 1)];
                for &member in members {
                    inside[member as usize] = true;
                }
                Some((axis, inside))
            })
            .collect();
        let pieces: Vec<Range<usize>> = ranges
            .into_iter()
            .flat_map(|range| {
                range
                    .clone()
                    .step_by(SCANNED_ROWS)
                    .map(move |start| start..(start + SCANNED_ROWS).min(range.end))
            })
            .collect();
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let scan = |pieces: &[Range<usize>]| -> Vec<usize> {
            pieces
                .iter()
                .cloned()
                .flatten()
                .filter(|&row| {
                    checks.iter().all(|(axis, inside)| {
                        inside
                            .get(self.layout.member(self.keys[row], *axis) as usize)
                            .copied()
                            .unwrap_or(false)
                    })
                })
                .collect()
        };
        let mut chunks = pieces.chunks(pieces.len().div_ceil(threads).max(1));
        let first = chunks.next().unwrap_or_default();
        let rows: Vec<usize> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunks.map(|chunk| scope.spawn(|| scan(chunk))).collect();
            let mut rows = scan(first);
            for handle in handles {
                rows.extend(handle.join().expect("a scan does not panic"));
            }
            rows
        });
        if rows.len() == self.len() {
            return self;
        }
        self.take(&rows)
    }

    pub(crate) fn changes_from(&self, old: &Columns) -> Vec<(Vec<u32>, Option<f64>)> {
        let mut changes = Vec::new();
        let (mut new, mut old_rows) = ((0..self.len()).peekable(), (0..old.len()).peekable());
        loop {
            match (new.peek(), old_rows.peek()) {
                (None, None) => break,
                (Some(_), None) => changes.push(self.cell(new.next())),
                (None, Some(_)) => {
                    let (coordinate, _) = old.cell(old_rows.next());
                    changes.push((coordinate, None));
                }
                (Some(&row), Some(&old_row)) => match self.keys[row].cmp(&old.keys[old_row]) {
                    std::cmp::Ordering::Less => changes.push(self.cell(new.next())),
                    std::cmp::Ordering::Greater => {
                        let (coordinate, _) = old.cell(old_rows.next());
                        changes.push((coordinate, None));
                    }
                    std::cmp::Ordering::Equal => {
                        old_rows.next();
                        let (coordinate, value) = self.cell(new.next());
                        if value.map(f64::to_bits) != Some(old.values[old_row].to_bits()) {
                            changes.push((coordinate, value));
                        }
                    }
                },
            }
        }
        changes
    }

    fn from_base(measure: &Measure) -> Columns {
        let axes = measure.definition().axes.clone();
        let base = measure.base();
        let members: Vec<Column> = (0..axes.len())
            .map(|axis| {
                base.column(axis)
                    .as_primitive::<UInt32Type>()
                    .values()
                    .clone()
            })
            .collect();
        let values = base
            .column(axes.len())
            .as_primitive::<Float64Type>()
            .values()
            .clone();
        if axes.is_sorted() {
            // Not Columns::from_keys: Measure::try_new already rejects unsorted or unnormalized bases.
            return Columns {
                axes,
                layout: measure.layout().clone(),
                keys: measure.keys().clone(),
                values,
                members: Some(members),
            };
        }
        let mut by_axis: Vec<(DimensionUlid, Column)> = axes.into_iter().zip(members).collect();
        by_axis.sort_by_key(|&(axis, _)| axis);
        let (axes, members): (Vec<DimensionUlid>, Vec<Column>) = by_axis.into_iter().unzip();
        let lengths: Vec<u32> = axes
            .iter()
            .map(|axis| {
                let position = measure
                    .definition()
                    .axes
                    .iter()
                    .position(|candidate| candidate == axis)
                    .expect("the columns keep the measure's axes");
                measure.layout().member(u64::MAX, position) + 1
            })
            .collect();
        let layout = KeyLayout::new(&lengths).expect("the measure's key fits the same widths");
        let keys = (0..values.len())
            .map(|row| layout.pack(members.iter().map(|column| column[row])))
            .collect();
        Columns::from_keys(axes, layout, keys, values.to_vec())
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
            .iter()
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
        Ok(Columns::from_keys(
            axes,
            layout,
            keys,
            columns.values.to_vec(),
        ))
    }

    pub(crate) fn negate(self) -> Columns {
        Columns {
            values: self
                .values
                .iter()
                .map(|&value| if value == 0.0 { 0.0 } else { -value })
                .collect(),
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
        apply: impl Fn(f64, f64) -> f64,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let positions: Vec<usize> = smaller
            .axes
            .iter()
            .map(|&axis| larger.position(axis))
            .collect();
        let index = Index::new(smaller, dimensions)?;
        let mut key = vec![0; positions.len()];
        let mut rows = Vec::new();
        let mut values = Vec::new();
        for (row, &larger_key) in larger.keys.iter().enumerate() {
            for (member, &position) in key.iter_mut().zip(&positions) {
                *member = larger.layout.member(larger_key, position);
            }
            if let Some(other) = index.find(&key)
                && let Some(value) = normalize(apply(larger.values[row], smaller.values[other]))
            {
                rows.push(row);
                values.push(value);
            }
        }
        if rows.len() == larger.len() {
            return Ok(Columns {
                axes: larger.axes.clone(),
                layout: larger.layout.clone(),
                keys: larger.keys.clone(),
                values: values.into(),
                members: larger.members.clone(),
            });
        }
        Ok(Columns {
            values: values.into(),
            ..larger.take(&rows)
        })
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
        let coordinates = creators[1..]
            .iter()
            .fold(creators[0].coordinates(), |union, creator| {
                union.union(creator)
            });
        let mut sources = terms
            .iter()
            .map(|(sign, term)| {
                let source = if term.axes.len() == axes.len() {
                    Source::Cursor(0)
                } else {
                    Source::Indexed(
                        Index::new(term, dimensions)?,
                        term.axes
                            .iter()
                            .map(|&axis| coordinates.position(axis))
                            .collect(),
                    )
                };
                Ok((*sign, term, source))
            })
            .collect::<Result<Vec<_>, EvaluateError>>()?;
        let mut key = Vec::new();
        let values: Vec<f64> = coordinates
            .keys
            .iter()
            .map(|&coordinate| {
                let mut total = 0.0;
                for (sign, term, source) in &mut sources {
                    let value =
                        match source {
                            Source::Cursor(next) => {
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
                                    coordinates.layout.member(coordinate, position)
                                }));
                                index.find(&key).map_or(0.0, |found| term.values[found])
                            }
                        };
                    match sign {
                        Sign::Plus => total += value,
                        Sign::Minus => total -= value,
                    }
                }
                total
            })
            .collect();
        drop(sources);
        Ok(Columns {
            values: values.into(),
            ..coordinates
        }
        .normalized())
    }

    fn coordinates(&self) -> Columns {
        Columns {
            axes: self.axes.clone(),
            layout: self.layout.clone(),
            keys: self.keys.clone(),
            values: ScalarBuffer::from(Vec::new()),
            members: self.members.clone(),
        }
    }

    fn union(self, other: &Columns) -> Columns {
        if self.axes == other.axes && self.keys.ptr_eq(&other.keys) {
            return self;
        }
        let (mine, theirs) = (&self.keys, &other.keys);
        let mut keys = Vec::with_capacity(mine.len().max(theirs.len()));
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
        Columns {
            keys: keys.into(),
            values: ScalarBuffer::from(Vec::new()),
            members: None,
            axes: self.axes,
            layout: self.layout,
        }
    }

    pub(crate) fn sum(
        &self,
        by: &[ByAxis<'_>],
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut outputs: Vec<&ByAxis<'_>> = by.iter().collect();
        outputs.sort_by_key(|axis| axis.output);
        let axes: Vec<DimensionUlid> = outputs.iter().map(|axis| axis.output).collect();
        let lengths = lengths(dimensions, &axes)?;
        let layout = layout_of(dimensions, &axes)?;
        if let Some((space, strides)) = dense_strides(&lengths) {
            let mut totals = vec![0.0; space];
            let mut seen = vec![false; space];
            'rows: for row in 0..self.len() {
                let mut slot = 0;
                for (axis, stride) in outputs.iter().zip(&strides) {
                    let Some(member) = axis.member(self, row) else {
                        continue 'rows;
                    };
                    slot += stride * member as usize;
                }
                totals[slot] += self.values[row];
                seen[slot] = true;
            }
            let slots: Vec<usize> = (0..space).filter(|&slot| seen[slot]).collect();
            let keys = slots
                .iter()
                .map(|&slot| {
                    layout.pack(
                        strides
                            .iter()
                            .zip(&lengths)
                            .map(|(stride, &len)| (slot / stride) as u32 % len),
                    )
                })
                .collect();
            let values = slots.iter().map(|&slot| totals[slot]).collect();
            return Ok(Columns::from_keys(axes, layout, keys, values));
        }
        let (kept, group_keys): (Vec<usize>, Vec<u64>) = (0..self.len())
            .filter_map(|row| {
                let members: Option<Vec<u32>> =
                    outputs.iter().map(|axis| axis.member(self, row)).collect();
                Some((row, layout.pack(members?)))
            })
            .unzip();
        let order = stable_order(&group_keys);
        let runs = runs(&group_keys, &order);
        let keys = runs.iter().map(|run| group_keys[run[0]]).collect();
        let values = runs
            .iter()
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
                members: self.members.clone(),
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
        let unpack = |position: usize| -> Column {
            self.keys
                .iter()
                .map(|&key| self.layout.member(key, position))
                .collect()
        };
        let mut arrays: Vec<ArrayRef>;
        if positions.is_sorted() {
            arrays = positions
                .iter()
                .map(|&position| {
                    let column = match &self.members {
                        Some(members) => members[position].clone(),
                        None => unpack(position),
                    };
                    Arc::new(UInt32Array::new(column, None)) as ArrayRef
                })
                .collect();
            arrays.push(Arc::new(Float64Array::new(self.values.clone(), None)));
            let base = RecordBatch::try_new(Arc::new(definition.schema()), arrays)
                .expect("the columns follow the result's schema");
            return Measure::try_new_with_keys(definition, base, Some(self.keys), dimensions);
        }
        let columns: Vec<Column> = positions.iter().map(|&position| unpack(position)).collect();
        let mut rows: Vec<usize> = (0..self.len()).collect();
        rows.sort_by(|&left, &right| {
            columns
                .iter()
                .map(|column| column[left])
                .cmp(columns.iter().map(|column| column[right]))
        });
        arrays = columns
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_terms_on_the_same_coordinates_keeps_their_keys() {
        let axis = DimensionUlid(ulid::Ulid(1));
        let layout = KeyLayout::new(&[8]).expect("3 bits fit in a key");
        let sales = Columns::from_keys(
            vec![axis],
            layout.clone(),
            vec![0, 2, 5],
            vec![3.0, 4.0, 5.0],
        );
        let cost = Columns {
            values: vec![1.0, 1.0, 1.0].into(),
            ..sales.coordinates()
        };
        let keys = sales.keys.clone();

        let profit = Columns::add(vec![(Sign::Plus, sales), (Sign::Minus, cost)], &[])
            .expect("both terms have every axis");

        assert!(profit.keys.ptr_eq(&keys));
        assert_eq!(&profit.values[..], &[2.0, 3.0, 4.0]);
    }
}
