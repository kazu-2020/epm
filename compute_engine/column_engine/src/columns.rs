use std::cmp::Ordering;
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
use measure::{Measure, MeasureDefinition, MeasureError, normalize};

use crate::region::Region;

const DENSE_LIMIT: usize = 1 << 22;
// Not one binary search per member above this many: walking the runs is faster.
const SEARCHED_MEMBERS: usize = 64;
// Not one range per thread: a single large range would leave the other threads idle.
const SCANNED_ROWS: usize = 1 << 20;

type Column = ScalarBuffer<u32>;

pub(crate) struct Columns {
    axes: Vec<DimensionUlid>,
    members: Vec<Column>,
    values: ScalarBuffer<f64>,
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

fn compare(left: &[&[u32]], left_row: usize, right: &[&[u32]], right_row: usize) -> Ordering {
    left.iter()
        .map(|column| column[left_row])
        .cmp(right.iter().map(|column| column[right_row]))
}

fn in_order(columns: &[&[u32]], len: usize) -> bool {
    (1..len).all(|row| compare(columns, row - 1, columns, row) != Ordering::Greater)
}

fn sorted_rows(columns: &[&[u32]], len: usize) -> Vec<usize> {
    let mut rows: Vec<usize> = (0..len).collect();
    if !in_order(columns, len) {
        // Not sort_unstable_by: SUM adds each group in coordinate order.
        rows.sort_by(|&left, &right| compare(columns, left, columns, right));
    }
    rows
}

fn runs<'r>(columns: &[&[u32]], rows: &'r [usize]) -> Vec<&'r [usize]> {
    rows.chunk_by(|&left, &right| compare(columns, left, columns, right) == Ordering::Equal)
        .collect()
}

fn narrow(
    columns: &[&[u32]],
    constraints: &[Option<&BTreeSet<u32>>],
    axis: usize,
    rows: Range<usize>,
    kept: &mut Vec<Range<usize>>,
) {
    let members = match constraints.get(axis) {
        Some(&Some(members)) if searched(members) => members,
        _ => return kept.push(rows),
    };
    let column = &columns[axis][rows.clone()];
    for &member in members {
        let start = column.partition_point(|&candidate| candidate < member);
        let end = column.partition_point(|&candidate| candidate <= member);
        narrow(
            columns,
            constraints,
            axis + 1,
            rows.start + start..rows.start + end,
            kept,
        );
    }
}

fn searched(members: &BTreeSet<u32>) -> bool {
    members.len() <= SEARCHED_MEMBERS
}

fn slices(columns: &[Column]) -> Vec<&[u32]> {
    columns.iter().map(|column| &column[..]).collect()
}

fn gather(columns: &[&[u32]], rows: &[usize]) -> Vec<Column> {
    columns
        .iter()
        .map(|column| rows.iter().map(|&row| column[row]).collect())
        .collect()
}

fn lengths(dimensions: &[&Dimension], axes: &[DimensionUlid]) -> Result<Vec<u32>, EvaluateError> {
    axes.iter()
        .map(|&axis| find_dimension(dimensions, axis).map(Dimension::len))
        .collect()
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

struct Index<'c> {
    columns: &'c Columns,
    dense: Option<(Vec<usize>, Vec<u32>)>,
}

impl<'c> Index<'c> {
    fn new(columns: &'c Columns, dimensions: &[&Dimension]) -> Result<Self, EvaluateError> {
        let dense = dense_strides(&lengths(dimensions, &columns.axes)?).map(|(space, strides)| {
            let mut rows = vec![u32::MAX; space];
            for row in 0..columns.len() {
                let slot: usize = strides
                    .iter()
                    .zip(&columns.members)
                    .map(|(stride, column)| stride * column[row] as usize)
                    .sum();
                rows[slot] = row as u32;
            }
            (strides, rows)
        });
        Ok(Index { columns, dense })
    }

    fn find(&self, key: &[u32]) -> Option<usize> {
        match &self.dense {
            Some((strides, rows)) => {
                let slot: usize = strides
                    .iter()
                    .zip(key)
                    .map(|(stride, &member)| stride * member as usize)
                    .sum();
                let row = rows[slot];
                (row != u32::MAX).then_some(row as usize)
            }
            None => self.columns.row_of(key),
        }
    }
}

impl ByAxis<'_> {
    fn member(&self, columns: &[Column], row: usize) -> Option<u32> {
        let member = columns[self.source][row];
        match self.attribute {
            None => Some(member),
            Some(attribute) => attribute.value(MemberId(member)).map(|target| target.0),
        }
    }
}

impl Columns {
    fn new(
        axes: Vec<DimensionUlid>,
        members: Vec<Column>,
        values: impl Into<ScalarBuffer<f64>>,
    ) -> Columns {
        let values: ScalarBuffer<f64> = values.into();
        let mut by_axis: Vec<(DimensionUlid, Column)> = axes.into_iter().zip(members).collect();
        by_axis.sort_by_key(|&(axis, _)| axis);
        let (axes, members): (Vec<DimensionUlid>, Vec<Column>) = by_axis.into_iter().unzip();
        let columns = slices(&members);
        if values.iter().all(|&value| is_normalized(value)) && in_order(&columns, values.len()) {
            return Columns {
                axes,
                members,
                values,
            };
        }
        let mut rows: Vec<usize> = (0..values.len())
            .filter(|&row| normalize(values[row]).is_some())
            .collect();
        rows.sort_unstable_by(|&left, &right| compare(&columns, left, &columns, right));
        Columns {
            axes,
            values: rows
                .iter()
                .filter_map(|&row| normalize(values[row]))
                .collect(),
            members: gather(&columns, &rows),
        }
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn columns(&self) -> Vec<&[u32]> {
        slices(&self.members)
    }

    pub(crate) fn number(value: f64) -> Columns {
        Columns::new(Vec::new(), Vec::new(), vec![value])
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
                changes.insert(key, value);
            }
        }
        base.with_changes(changes)
    }

    fn with_changes(self, changes: BTreeMap<Vec<u32>, Option<f64>>) -> Columns {
        if changes.is_empty() {
            return self;
        }
        let columns = self.columns();
        let mut cells: Vec<(Vec<u32>, f64)> = Vec::new();
        let (mut rows, mut changes) = ((0..self.len()).peekable(), changes.into_iter().peekable());
        loop {
            let (coordinate, value) = match (rows.peek(), changes.peek()) {
                (None, None) => break,
                (Some(_), None) => self.cell(&columns, rows.next()),
                (None, Some(_)) => changes.next().expect("peeked above"),
                (Some(&row), Some((key, _))) => {
                    match columns
                        .iter()
                        .map(|column| column[row])
                        .cmp(key.iter().copied())
                    {
                        Ordering::Less => self.cell(&columns, rows.next()),
                        Ordering::Greater => changes.next().expect("peeked above"),
                        Ordering::Equal => {
                            rows.next();
                            changes.next().expect("peeked above")
                        }
                    }
                }
            };
            if let Some(value) = value {
                cells.push((coordinate, value));
            }
        }
        Columns {
            members: (0..self.axes.len())
                .map(|axis| {
                    cells
                        .iter()
                        .map(|(coordinate, _)| coordinate[axis])
                        .collect()
                })
                .collect(),
            values: cells.iter().map(|&(_, value)| value).collect(),
            axes: self.axes,
        }
    }

    fn cell(&self, columns: &[&[u32]], row: Option<usize>) -> (Vec<u32>, Option<f64>) {
        let row = row.expect("peeked above");
        (
            columns.iter().map(|column| column[row]).collect(),
            Some(self.values[row]),
        )
    }

    pub(crate) fn restrict(self, region: &Region) -> Columns {
        let constraints: Vec<Option<&BTreeSet<u32>>> =
            self.axes.iter().map(|&axis| region.members(axis)).collect();
        if constraints.iter().all(Option::is_none) {
            return self;
        }
        let columns = self.columns();
        let mut ranges = Vec::new();
        narrow(&columns, &constraints, 0, 0..self.len(), &mut ranges);
        let checks: Vec<(&[u32], Vec<bool>)> = constraints
            .iter()
            .zip(&columns)
            .filter_map(|(constraint, &column)| {
                let members = (*constraint)?;
                let mut inside = vec![false; members.last().map_or(0, |&last| last as usize + 1)];
                for &member in members {
                    inside[member as usize] = true;
                }
                Some((column, inside))
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
                    checks.iter().all(|(column, inside)| {
                        inside.get(column[row] as usize).copied().unwrap_or(false)
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
        Columns {
            members: gather(&self.columns(), &rows),
            values: rows.iter().map(|&row| self.values[row]).collect(),
            axes: self.axes,
        }
    }

    pub(crate) fn changes_from(&self, old: &Columns) -> Vec<(Vec<u32>, Option<f64>)> {
        let (new_columns, old_columns) = (self.columns(), old.columns());
        let mut changes = Vec::new();
        let (mut new, mut old_rows) = ((0..self.len()).peekable(), (0..old.len()).peekable());
        loop {
            match (new.peek(), old_rows.peek()) {
                (None, None) => break,
                (Some(_), None) => {
                    let (coordinate, value) = self.cell(&new_columns, new.next());
                    changes.push((coordinate, value));
                }
                (None, Some(_)) => {
                    let (coordinate, _) = old.cell(&old_columns, old_rows.next());
                    changes.push((coordinate, None));
                }
                (Some(&row), Some(&old_row)) => {
                    match compare(&new_columns, row, &old_columns, old_row) {
                        Ordering::Less => {
                            changes.push(self.cell(&new_columns, new.next()));
                        }
                        Ordering::Greater => {
                            let (coordinate, _) = old.cell(&old_columns, old_rows.next());
                            changes.push((coordinate, None));
                        }
                        Ordering::Equal => {
                            old_rows.next();
                            let (coordinate, value) = self.cell(&new_columns, new.next());
                            if value.map(f64::to_bits) != Some(old.values[old_row].to_bits()) {
                                changes.push((coordinate, value));
                            }
                        }
                    }
                }
            }
        }
        changes
    }

    fn from_base(measure: &Measure) -> Columns {
        let axes = measure.definition().axes.clone();
        let base = measure.base();
        let members = (0..axes.len())
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
            // Not Columns::new: Measure::try_new already rejects unsorted or unnormalized bases.
            return Columns {
                axes,
                members,
                values,
            };
        }
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
        let mut region = Region::default();
        region.constrain(dimension, members.iter().map(|member| member.0).collect());
        let mut columns = self.restrict(&region);
        if !keeps_axis {
            let position = columns.position(dimension);
            columns.axes.remove(position);
            columns.members.remove(position);
        }
        columns
    }

    pub(crate) fn negate(self) -> Columns {
        let values: Vec<f64> = self.values.iter().map(|value| -value).collect();
        Columns::new(self.axes, self.members, values)
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
        let left_key: Vec<&[u32]> = shared
            .iter()
            .map(|&(position, _)| &left.members[position][..])
            .collect();
        let right_key: Vec<&[u32]> = shared
            .iter()
            .map(|&(_, position)| &right.members[position][..])
            .collect();
        let left_rows = sorted_rows(&left_key, left.len());
        let right_rows = sorted_rows(&right_key, right.len());
        let mut axes = left.axes.clone();
        axes.extend(extra.iter().map(|&position| right.axes[position]));
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); axes.len()];
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
        Ok(Columns::new(
            axes,
            members.into_iter().map(Column::from).collect(),
            values,
        ))
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
        for row in 0..larger.len() {
            for (member, &position) in key.iter_mut().zip(&positions) {
                *member = larger.members[position][row];
            }
            if let Some(other) = index.find(&key)
                && let Some(value) = normalize(apply(larger.values[row], smaller.values[other]))
            {
                rows.push(row);
                values.push(value);
            }
        }
        let members = if rows.len() == larger.len() {
            larger.members.clone()
        } else {
            gather(&larger.columns(), &rows)
        };
        Ok(Columns {
            axes: larger.axes.clone(),
            members,
            values: values.into(),
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
        let columns = coordinates.columns();
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
                Ok((*sign, term.columns(), term, source))
            })
            .collect::<Result<Vec<_>, EvaluateError>>()?;
        let mut key = Vec::new();
        let values: Vec<f64> = (0..coordinates.len())
            .map(|row| {
                let mut total = 0.0;
                for (sign, term_columns, term, source) in &mut sources {
                    let value = match source {
                        Source::Cursor(next) => {
                            if *next < term.len()
                                && compare(term_columns, *next, &columns, row) == Ordering::Equal
                            {
                                *next += 1;
                                term.values[*next - 1]
                            } else {
                                0.0
                            }
                        }
                        Source::Indexed(index, positions) => {
                            key.clear();
                            key.extend(positions.iter().map(|&position| columns[position][row]));
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
        drop(columns);
        Ok(Columns::new(coordinates.axes, coordinates.members, values))
    }

    fn coordinates(&self) -> Columns {
        Columns {
            axes: self.axes.clone(),
            members: self.members.clone(),
            values: vec![0.0; self.len()].into(),
        }
    }

    fn union(self, other: &Columns) -> Columns {
        let (mine, theirs) = (self.columns(), other.columns());
        let mut rows = Vec::new();
        let (mut own, mut their) = ((0..self.len()).peekable(), (0..other.len()).peekable());
        loop {
            let (from_mine, row) = match (own.peek(), their.peek()) {
                (None, None) => break,
                (Some(_), None) => (true, own.next()),
                (None, Some(_)) => (false, their.next()),
                (Some(&left), Some(&right)) => match compare(&mine, left, &theirs, right) {
                    Ordering::Less => (true, own.next()),
                    Ordering::Greater => (false, their.next()),
                    Ordering::Equal => {
                        their.next();
                        (true, own.next())
                    }
                },
            };
            rows.push((from_mine, row.expect("peeked above")));
        }
        let members = (0..self.axes.len())
            .map(|axis| {
                rows.iter()
                    .map(|&(from_mine, row)| {
                        if from_mine {
                            mine[axis][row]
                        } else {
                            theirs[axis][row]
                        }
                    })
                    .collect()
            })
            .collect();
        Columns {
            values: vec![0.0; rows.len()].into(),
            members,
            axes: self.axes.clone(),
        }
    }

    fn row_of(&self, coordinate: &[u32]) -> Option<usize> {
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
                Ordering::Equal => return Some(middle),
            }
        }
        None
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
        if let Some((space, strides)) = dense_strides(&lengths) {
            let mut totals = vec![0.0; space];
            let mut seen = vec![false; space];
            'rows: for row in 0..self.len() {
                let mut slot = 0;
                for (axis, stride) in outputs.iter().zip(&strides) {
                    let Some(member) = axis.member(&self.members, row) else {
                        continue 'rows;
                    };
                    slot += stride * member as usize;
                }
                totals[slot] += self.values[row];
                seen[slot] = true;
            }
            let slots: Vec<usize> = (0..space).filter(|&slot| seen[slot]).collect();
            let members = strides
                .iter()
                .zip(&lengths)
                .map(|(stride, &len)| {
                    slots
                        .iter()
                        .map(|slot| (slot / stride) as u32 % len)
                        .collect()
                })
                .collect();
            let values: Vec<f64> = slots.iter().map(|&slot| totals[slot]).collect();
            return Ok(Columns::new(axes, members, values));
        }
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
        let order = sorted_rows(&group_columns, kept.len());
        let runs = runs(&group_columns, &order);
        let members = group_columns
            .iter()
            .map(|column| runs.iter().map(|run| column[run[0]]).collect())
            .collect();
        let values: Vec<f64> = runs
            .iter()
            .map(|run| {
                run.iter().fold(0.0, |total, &group_row| {
                    total + self.values[kept[group_row]]
                })
            })
            .collect();
        Ok(Columns::new(
            groups.into_iter().map(|(axis, _)| axis).collect(),
            members,
            values,
        ))
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
        let positions: Vec<usize> = definition
            .axes
            .iter()
            .map(|&axis| self.position(axis))
            .collect();
        let columns: Vec<&[u32]> = positions
            .iter()
            .map(|&position| &self.members[position][..])
            .collect();
        let mut arrays: Vec<ArrayRef>;
        let values;
        if positions.is_sorted() {
            arrays = positions
                .iter()
                .map(|&position| {
                    Arc::new(UInt32Array::new(self.members[position].clone(), None)) as ArrayRef
                })
                .collect();
            values = self.values.clone();
        } else {
            let rows = sorted_rows(&columns, self.len());
            arrays = gather(&columns, &rows)
                .into_iter()
                .map(|column| Arc::new(UInt32Array::new(column, None)) as ArrayRef)
                .collect();
            values = rows.iter().map(|&row| self.values[row]).collect();
        }
        arrays.push(Arc::new(Float64Array::new(values, None)));
        let base = RecordBatch::try_new(Arc::new(definition.schema()), arrays)
            .expect("the columns follow the result's schema");
        Measure::try_new(definition, base, dimensions)
    }
}
