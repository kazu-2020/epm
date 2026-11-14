use std::collections::BTreeSet;
use std::iter;
use std::ops::Range;

use arrow_buffer::ScalarBuffer;
use dimension::{Dimension, DimensionGlobalId, MemberIndex, ReferenceAttribute};
use formula::{EvaluateError, ProductOperator, Sign};
use measure::{CoordinatePacking, Measure, MeasureDefinition, MeasureError, normalize};
use rayon::prelude::*;

use crate::region::Region;

mod rows;

const DENSE_LIMIT: usize = 1 << 22;
// Not one binary search per member above this many: walking the runs is faster.
const SEARCHED_MEMBERS: usize = 64;
// Not one range per thread: a single large range would leave the other threads idle.
const SCANNED_ROWS: usize = 1 << 20;
// Not smaller pieces: below this many rows the cost of handing out work is larger than the work.
const PARALLEL_ROWS: usize = 1 << 14;

#[derive(Clone)]
pub(crate) struct Columns {
    axes: Vec<DimensionGlobalId>,
    packing: CoordinatePacking,
    packed_coordinates: ScalarBuffer<u64>,
    values: ScalarBuffer<f64>,
}

pub(crate) struct ByAxis<'d> {
    pub(crate) output: DimensionGlobalId,
    pub(crate) source: usize,
    pub(crate) attribute: Option<ReferenceAttribute<'d>>,
}

enum Source<'c> {
    Cursor,
    Indexed(Index<'c>, Vec<usize>),
}

pub(crate) fn find_dimension<'d>(
    dimensions: &[&'d Dimension],
    global_id: DimensionGlobalId,
) -> Result<&'d Dimension, EvaluateError> {
    dimensions
        .iter()
        .find(|dimension| dimension.global_id() == global_id)
        .copied()
        .ok_or(EvaluateError::MissingDimension(global_id))
}

pub(crate) fn sort_in_packing_order<T>(
    items: &mut [T],
    axis: impl Fn(&T) -> DimensionGlobalId,
    dimensions: &[&Dimension],
) -> Result<(), EvaluateError> {
    for item in items.iter() {
        find_dimension(dimensions, axis(item))?;
    }
    items.sort_by_cached_key(|item| {
        find_dimension(dimensions, axis(item))
            .expect("every axis was found above")
            .packing_order()
    });
    Ok(())
}

fn member_counts(
    dimensions: &[&Dimension],
    axes: &[DimensionGlobalId],
) -> Result<Vec<u32>, EvaluateError> {
    axes.iter()
        .map(|&axis| find_dimension(dimensions, axis).map(Dimension::len))
        .collect()
}

pub(crate) fn packing_of(
    dimensions: &[&Dimension],
    axes: &[DimensionGlobalId],
) -> Result<CoordinatePacking, EvaluateError> {
    CoordinatePacking::new(&member_counts(dimensions, axes)?).ok_or(EvaluateError::PackingTooWide)
}

fn dense_strides(member_counts: &[u32]) -> Option<(usize, Vec<usize>)> {
    let mut strides = vec![0; member_counts.len()];
    let mut space: usize = 1;
    for (stride, &member_count) in strides.iter_mut().zip(member_counts).rev() {
        *stride = space;
        space = space.checked_mul(member_count as usize)?;
    }
    (space <= DENSE_LIMIT).then_some((space, strides))
}

fn is_normalized(value: f64) -> bool {
    normalize(value).map(f64::to_bits) == Some(value.to_bits())
}

fn all_normalized(values: &[f64]) -> bool {
    values
        .par_chunks(PARALLEL_ROWS)
        .all(|piece| piece.iter().copied().all(is_normalized))
}

fn stable_order(packed_coordinates: &[u64]) -> Vec<usize> {
    let mut rows: Vec<usize> = (0..packed_coordinates.len()).collect();
    if !rows::ascending(packed_coordinates) {
        // Not sort_unstable: SUM adds each group in coordinate order.
        rows.par_sort_by_key(|&row| packed_coordinates[row]);
    }
    rows
}

fn runs<'r>(packed_coordinates: &[u64], rows: &'r [usize]) -> Vec<&'r [usize]> {
    rows.chunk_by(|&left, &right| packed_coordinates[left] == packed_coordinates[right])
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
        let dense =
            dense_strides(&member_counts(dimensions, &columns.axes)?).map(|(space, strides)| {
                let mut rows = vec![u32::MAX; space];
                for (row, &packed) in columns.packed_coordinates.iter().enumerate() {
                    let slot: usize = strides
                        .iter()
                        .zip(columns.packing.unpack(packed))
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
                .packed_coordinates
                .binary_search(&self.columns.packing.pack(members.iter().copied()))
                .ok(),
        }
    }
}

impl ByAxis<'_> {
    fn member(&self, columns: &Columns, row: usize) -> Option<u32> {
        let member = columns
            .packing
            .member(columns.packed_coordinates[row], self.source);
        match self.attribute {
            None => Some(member),
            Some(attribute) => attribute.value(MemberIndex(member)).map(|target| target.0),
        }
    }
}

impl Columns {
    fn build(
        axes: Vec<DimensionGlobalId>,
        members: Vec<Vec<u32>>,
        values: Vec<f64>,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut by_axis: Vec<(DimensionGlobalId, Vec<u32>)> =
            axes.into_iter().zip(members).collect();
        sort_in_packing_order(&mut by_axis, |&(axis, _)| axis, dimensions)?;
        let (axes, members): (Vec<DimensionGlobalId>, Vec<Vec<u32>>) = by_axis.into_iter().unzip();
        let packing = packing_of(dimensions, &axes)?;
        let packed_coordinates: Vec<u64> = (0..values.len())
            .into_par_iter()
            .with_min_len(PARALLEL_ROWS)
            .map(|row| packing.pack(members.iter().map(|column| column[row])))
            .collect();
        Ok(Columns::from_packed(
            axes,
            packing,
            packed_coordinates,
            values,
        ))
    }

    fn from_packed(
        axes: Vec<DimensionGlobalId>,
        packing: CoordinatePacking,
        packed_coordinates: Vec<u64>,
        values: impl Into<ScalarBuffer<f64>>,
    ) -> Columns {
        let values = values.into();
        if rows::ascending(&packed_coordinates) && all_normalized(&values) {
            return Columns {
                axes,
                packing,
                packed_coordinates: packed_coordinates.into(),
                values,
            };
        }
        let rows: Vec<usize> = stable_order(&packed_coordinates)
            .into_par_iter()
            .with_min_len(PARALLEL_ROWS)
            .filter(|&row| normalize(values[row]).is_some())
            .collect();
        Columns {
            axes,
            packing,
            packed_coordinates: rows::gather(&rows, &packed_coordinates).into(),
            values: rows
                .par_iter()
                .with_min_len(PARALLEL_ROWS)
                .filter_map(|&row| normalize(values[row]))
                .collect::<Vec<f64>>()
                .into(),
        }
    }

    fn normalized(self) -> Columns {
        if all_normalized(&self.values) {
            return self;
        }
        self.select(&rows::pieces(self.len()), |row| {
            normalize(self.values[row]).is_some()
        })
    }

    fn select(&self, pieces: &[Range<usize>], keep: impl Fn(usize) -> bool + Sync) -> Columns {
        let counts: Vec<usize> = pieces
            .par_iter()
            .map(|piece| piece.clone().filter(|&row| keep(row)).count())
            .collect();
        if counts.iter().sum::<usize>() == self.len() {
            return self.clone();
        }
        self.copy_rows(&counts, |piece| {
            pieces[piece].clone().filter(|&row| keep(row))
        })
    }

    fn copy_rows<Rows: Iterator<Item = usize>>(
        &self,
        counts: &[usize],
        piece_rows: impl Fn(usize) -> Rows + Sync,
    ) -> Columns {
        let (packed_coordinates, values) =
            rows::copy_rows(&self.packed_coordinates, &self.values, counts, piece_rows);
        Columns {
            axes: self.axes.clone(),
            packing: self.packing.clone(),
            packed_coordinates: packed_coordinates.into(),
            values: values.into(),
        }
    }

    fn len(&self) -> usize {
        self.packed_coordinates.len()
    }

    pub(crate) fn number(value: f64) -> Columns {
        Columns::from_packed(
            Vec::new(),
            CoordinatePacking::new(&[]).expect("no axes fit in a packed coordinate"),
            vec![0],
            vec![value],
        )
    }

    pub(crate) fn from_measure(measure: &Measure) -> Columns {
        Columns::from_base(&measure.compact())
    }

    pub(crate) fn from_measure_in(measure: &Measure, region: &Region) -> Columns {
        let base = Columns::from_base(measure).restrict(region);
        let (packed_coordinates, changes) = measure.merged_changes();
        let inside: Vec<(u64, Option<f64>)> = packed_coordinates
            .iter()
            .zip(changes)
            .filter(|&(&packed, _)| {
                base.axes.iter().enumerate().all(|(position, &axis)| {
                    region.members(axis).is_none_or(|members| {
                        members.contains(&base.packing.member(packed, position))
                    })
                })
            })
            .map(|(&packed, change)| (packed, change))
            .collect();
        base.with_changes(inside)
    }

    fn with_changes(self, changes: Vec<(u64, Option<f64>)>) -> Columns {
        if changes.is_empty() {
            return self;
        }
        let mut packed_coordinates = Vec::new();
        let mut values = Vec::new();
        let (mut rows, mut changes) = ((0..self.len()).peekable(), changes.into_iter().peekable());
        loop {
            let (packed, value) = match (rows.peek(), changes.peek()) {
                (None, None) => break,
                (Some(_), None) => self.entry(rows.next()),
                (None, Some(_)) => changes.next().expect("peeked above"),
                (Some(&row), Some(&(packed, _))) => match self.packed_coordinates[row].cmp(&packed)
                {
                    std::cmp::Ordering::Less => self.entry(rows.next()),
                    std::cmp::Ordering::Greater => changes.next().expect("peeked above"),
                    std::cmp::Ordering::Equal => {
                        rows.next();
                        changes.next().expect("peeked above")
                    }
                },
            };
            if let Some(value) = value {
                packed_coordinates.push(packed);
                values.push(value);
            }
        }
        Columns {
            axes: self.axes,
            packing: self.packing,
            packed_coordinates: packed_coordinates.into(),
            values: values.into(),
        }
    }

    fn entry(&self, row: Option<usize>) -> (u64, Option<f64>) {
        let row = row.expect("peeked above");
        (self.packed_coordinates[row], Some(self.values[row]))
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
        let packed_coordinates = &self.packed_coordinates[rows.clone()];
        for &member in members {
            let start = packed_coordinates
                .partition_point(|&packed| self.packing.member(packed, axis) < member);
            let end = packed_coordinates
                .partition_point(|&packed| self.packing.member(packed, axis) <= member);
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
        // Not one bitmap per axis for a single member: one mask and compare over the whole packed coordinate tests them all.
        let (mut mask, mut pattern) = (0, 0);
        let mut checks: Vec<(usize, Vec<bool>)> = Vec::new();
        for (axis, constraint) in constraints.iter().enumerate() {
            let Some(members) = *constraint else {
                continue;
            };
            if members.len() == 1
                && let Some(&member) = members.first()
                && self.packing.fits(axis, member)
            {
                mask |= self.packing.axis_mask(axis);
                pattern |= self.packing.place(axis, member);
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
        // Not select: it reads every packed coordinate twice, and an edit keeps a few rows out of millions, so the second read is the cost.
        let kept: Vec<Vec<usize>> = pieces
            .par_iter()
            .map(|piece| {
                self.packed_coordinates[piece.clone()]
                    .iter()
                    .enumerate()
                    .filter(|&(_, &packed)| {
                        packed & mask == pattern
                            && checks.iter().all(|(axis, inside)| {
                                inside
                                    .get(self.packing.member(packed, *axis) as usize)
                                    .copied()
                                    .unwrap_or(false)
                            })
                    })
                    .map(|(offset, _)| piece.start + offset)
                    .collect()
            })
            .collect();
        let counts: Vec<usize> = kept.iter().map(Vec::len).collect();
        if counts.iter().sum::<usize>() == self.len() {
            return self;
        }
        self.copy_rows(&counts, |piece| kept[piece].iter().copied())
    }

    pub(crate) fn changes_from(&self, old: &Columns) -> (Vec<u64>, Vec<Option<f64>>) {
        let (mut packed_coordinates, mut changes) = (Vec::new(), Vec::new());
        let (mut new, mut old_row) = (0, 0);
        while new < self.len() || old_row < old.len() {
            let order = match (new < self.len(), old_row < old.len()) {
                (true, true) => self.packed_coordinates[new].cmp(&old.packed_coordinates[old_row]),
                (true, false) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            match order {
                std::cmp::Ordering::Less => {
                    packed_coordinates.push(self.packed_coordinates[new]);
                    changes.push(Some(self.values[new]));
                    new += 1;
                }
                std::cmp::Ordering::Greater => {
                    packed_coordinates.push(old.packed_coordinates[old_row]);
                    changes.push(None);
                    old_row += 1;
                }
                std::cmp::Ordering::Equal => {
                    if self.values[new].to_bits() != old.values[old_row].to_bits() {
                        packed_coordinates.push(self.packed_coordinates[new]);
                        changes.push(Some(self.values[new]));
                    }
                    new += 1;
                    old_row += 1;
                }
            }
        }
        (packed_coordinates, changes)
    }

    fn from_base(measure: &Measure) -> Columns {
        // Not Columns::from_packed: Measure already rejects unsorted or unnormalized cells.
        Columns {
            axes: measure.axes_in_packing_order().to_vec(),
            packing: measure.packing().clone(),
            packed_coordinates: measure.base_packed_coordinates().clone(),
            values: measure.base_values().clone(),
        }
    }

    pub(crate) fn position(&self, dimension: DimensionGlobalId) -> usize {
        self.axes
            .iter()
            .position(|&axis| axis == dimension)
            .expect("the columns have every axis asked for")
    }

    pub(crate) fn filter(
        self,
        dimension: DimensionGlobalId,
        members: &[MemberIndex],
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
        let packing = packing_of(dimensions, &axes)?;
        let packed_coordinates = columns
            .packed_coordinates
            .par_iter()
            .with_min_len(PARALLEL_ROWS)
            .map(|&packed| {
                packing.pack(
                    columns
                        .packing
                        .unpack(packed)
                        .enumerate()
                        .filter(|&(axis, _)| axis != position)
                        .map(|(_, member)| member),
                )
            })
            .collect();
        Ok(Columns::from_packed(
            axes,
            packing,
            packed_coordinates,
            columns.values,
        ))
    }

    pub(crate) fn product(
        operator: ProductOperator,
        left: &Columns,
        right: &Columns,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let apply: fn(f64, f64) -> f64 = match operator {
            ProductOperator::Multiply => |left, right| left * right,
            ProductOperator::Divide => |left, right| left / right,
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
        let shared_packing = packing_of(
            dimensions,
            &shared
                .iter()
                .map(|&(position, _)| left.axes[position])
                .collect::<Vec<_>>(),
        )?;
        let project = |columns: &Columns, positions: Vec<usize>| -> Vec<u64> {
            columns
                .packed_coordinates
                .iter()
                .map(|&packed| {
                    shared_packing.pack(
                        positions
                            .iter()
                            .map(|&position| columns.packing.member(packed, position)),
                    )
                })
                .collect()
        };
        let left_packed = project(left, shared.iter().map(|&(position, _)| position).collect());
        let right_packed = project(
            right,
            shared.iter().map(|&(_, position)| position).collect(),
        );
        let left_rows = stable_order(&left_packed);
        let right_rows = stable_order(&right_packed);
        let mut axes = left.axes.clone();
        axes.extend(extra.iter().map(|&position| right.axes[position]));
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); axes.len()];
        let mut values = Vec::new();
        let mut left_runs = runs(&left_packed, &left_rows).into_iter().peekable();
        let mut right_runs = runs(&right_packed, &right_rows).into_iter().peekable();
        while let (Some(&left_run), Some(&right_run)) = (left_runs.peek(), right_runs.peek()) {
            match left_packed[left_run[0]].cmp(&right_packed[right_run[0]]) {
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
                                .zip(left.packing.unpack(left.packed_coordinates[left_row]))
                            {
                                column.push(member);
                            }
                            for (column, &position) in from_right.iter_mut().zip(&extra) {
                                column.push(
                                    right
                                        .packing
                                        .member(right.packed_coordinates[right_row], position),
                                );
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
            .packed_coordinates
            .par_iter()
            .zip(&larger.values[..])
            .with_min_len(PARALLEL_ROWS)
            .map_init(
                || vec![0; positions.len()],
                |packed, (&larger_packed, &value)| {
                    for (member, &position) in packed.iter_mut().zip(&positions) {
                        *member = larger.packing.member(larger_packed, position);
                    }
                    index
                        .find(packed)
                        .and_then(|other| normalize(apply(value, smaller.values[other])))
                        .unwrap_or(f64::NAN)
                },
            )
            .collect();
        let columns = Columns {
            axes: larger.axes.clone(),
            packing: larger.packing.clone(),
            packed_coordinates: larger.packed_coordinates.clone(),
            values: values.into(),
        };
        if !columns.values.par_iter().any(|value| value.is_nan()) {
            return Ok(columns);
        }
        Ok(columns.select(&rows::pieces(columns.len()), |row| {
            !columns.values[row].is_nan()
        }))
    }

    pub(crate) fn add(
        terms: Vec<(Sign, Columns)>,
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        let mut axes: Vec<DimensionGlobalId> = terms
            .iter()
            .flat_map(|(_, term)| term.axes.iter().copied())
            .collect();
        sort_in_packing_order(&mut axes, |&axis| axis, dimensions)?;
        axes.dedup();
        let any_full = terms.iter().any(|(_, term)| term.axes.len() == axes.len());
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
        let packed_coordinates = creators[1..].iter().fold(
            first.packed_coordinates.clone(),
            |packed_coordinates, creator| {
                rows::union(packed_coordinates, &creator.packed_coordinates)
            },
        );
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
        let mut values = vec![0.0; packed_coordinates.len()];
        values
            .par_chunks_mut(PARALLEL_ROWS)
            .zip(packed_coordinates.par_chunks(PARALLEL_ROWS))
            .for_each(|(totals, coordinates)| {
                let mut cursors: Vec<usize> = sources
                    .iter()
                    .map(|(_, term, source)| match source {
                        Source::Cursor => term
                            .packed_coordinates
                            .partition_point(|&packed| packed < coordinates[0]),
                        Source::Indexed(..) => 0,
                    })
                    .collect();
                let mut packed = Vec::new();
                for (output, &coordinate) in totals.iter_mut().zip(coordinates) {
                    // Not `*output += value`: a read of the zeroed page before the write makes the kernel copy the zero page and flush the TLB of every core.
                    let mut total = 0.0;
                    for ((sign, term, source), next) in sources.iter().zip(&mut cursors) {
                        let value =
                            match source {
                                Source::Cursor => {
                                    if *next < term.len()
                                        && term.packed_coordinates[*next] == coordinate
                                    {
                                        *next += 1;
                                        term.values[*next - 1]
                                    } else {
                                        0.0
                                    }
                                }
                                Source::Indexed(index, positions) => {
                                    packed.clear();
                                    packed.extend(positions.iter().map(|&position| {
                                        first.packing.member(coordinate, position)
                                    }));
                                    index.find(&packed).map_or(0.0, |found| term.values[found])
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
            packing: first.packing.clone(),
            packed_coordinates,
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
        sort_in_packing_order(&mut outputs, |axis| axis.output, dimensions)?;
        let axes: Vec<DimensionGlobalId> = outputs.iter().map(|axis| axis.output).collect();
        let member_counts = member_counts(dimensions, &axes)?;
        let packing = packing_of(dimensions, &axes)?;
        if let Some((space, strides)) = dense_strides(&member_counts) {
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
            // Not the owner pass with one owner: it would store a slot for every row only to read them all back.
            if owners == 1 {
                for row in 0..self.len() {
                    if let Some(slot) = slot_of(row) {
                        totals[slot] += self.values[row];
                        seen[slot] = true;
                    }
                }
            } else {
                // Not partial totals per piece of rows: one thread adds each slot in row order (ADR 0001).
                // shortcut: each owner reads all rows (12 B a row), so this pass does not shrink with more threads; hand rows to owners again if the 32-core run shows it.
                let slots: Vec<u32> = (0..self.len())
                    .into_par_iter()
                    .with_min_len(PARALLEL_ROWS)
                    .map(|row| {
                        slot_of(row).map_or(u32::MAX, |slot| {
                            u32::try_from(slot)
                                .expect("dense_strides keeps space at or below DENSE_LIMIT")
                        })
                    })
                    .collect();
                totals
                    .par_chunks_mut(share)
                    .zip(seen.par_chunks_mut(share))
                    .enumerate()
                    .for_each(|(owner, (totals, seen))| {
                        // Not a write per row to the shared `totals` and `seen`: with few slots, owners share cache lines and wait for each other.
                        let mut own_totals = vec![0.0; totals.len()];
                        let mut own_seen = vec![false; seen.len()];
                        for (&slot, &value) in slots.iter().zip(&self.values[..]) {
                            let at = (slot as usize).wrapping_sub(owner * share);
                            if at < own_totals.len() {
                                own_totals[at] += value;
                                own_seen[at] = true;
                            }
                        }
                        totals.copy_from_slice(&own_totals);
                        seen.copy_from_slice(&own_seen);
                    });
            }
            let slots: Vec<usize> = (0..space)
                .into_par_iter()
                .with_min_len(PARALLEL_ROWS)
                .filter(|&slot| seen[slot])
                .collect();
            let packed_coordinates = slots
                .par_iter()
                .with_min_len(PARALLEL_ROWS)
                .map(|&slot| {
                    packing.pack(
                        strides
                            .iter()
                            .zip(&member_counts)
                            .map(|(stride, &member_count)| (slot / stride) as u32 % member_count),
                    )
                })
                .collect();
            let values = rows::gather(&slots, &totals);
            return Ok(Columns::from_packed(
                axes,
                packing,
                packed_coordinates,
                values,
            ));
        }
        let (kept, group_packed): (Vec<usize>, Vec<u64>) = (0..self.len())
            .into_par_iter()
            .with_min_len(PARALLEL_ROWS)
            .filter_map(|row| {
                let members: Option<Vec<u32>> =
                    outputs.iter().map(|axis| axis.member(self, row)).collect();
                Some((row, packing.pack(members?)))
            })
            .unzip();
        let order = stable_order(&group_packed);
        let runs = runs(&group_packed, &order);
        let packed_coordinates = runs.iter().map(|run| group_packed[run[0]]).collect();
        let values: Vec<f64> = runs
            .par_iter()
            .map(|run| {
                run.iter().fold(0.0, |total, &group_row| {
                    total + self.values[kept[group_row]]
                })
            })
            .collect();
        Ok(Columns::from_packed(
            axes,
            packing,
            packed_coordinates,
            values,
        ))
    }

    pub(crate) fn broadcast(
        &self,
        axes: &[DimensionGlobalId],
        dimensions: &[&Dimension],
    ) -> Result<Columns, EvaluateError> {
        if axes.iter().all(|axis| self.axes.contains(axis)) {
            return Ok(Columns {
                axes: self.axes.clone(),
                packing: self.packing.clone(),
                packed_coordinates: self.packed_coordinates.clone(),
                values: self.values.clone(),
            });
        }
        let mut spread_axes = self.axes.clone();
        let mut members: Vec<Vec<u32>> = (0..self.axes.len())
            .map(|axis| {
                self.packed_coordinates
                    .iter()
                    .map(|&packed| self.packing.member(packed, axis))
                    .collect()
            })
            .collect();
        let mut values = self.values.to_vec();
        for &axis in axes {
            if spread_axes.contains(&axis) {
                continue;
            }
            let member_count = find_dimension(dimensions, axis)?.len();
            let copies = member_count as usize;
            for column in &mut members {
                *column = column
                    .iter()
                    .flat_map(|&member| iter::repeat_n(member, copies))
                    .collect();
            }
            members.push((0..values.len()).flat_map(|_| 0..member_count).collect());
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
        Measure::try_from_packed(definition, self.packed_coordinates, self.values, dimensions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_packed_coordinates_still_drop_values_that_are_not_normalized() {
        let axis = DimensionGlobalId(ulid::Ulid(1));
        let packing = CoordinatePacking::new(&[8]).expect("3 bits fit in a packed coordinate");

        let columns = Columns::from_packed(vec![axis], packing, vec![0, 1], vec![f64::NAN, -0.0]);

        assert_eq!(&columns.packed_coordinates[..], &[1]);
        assert_eq!(columns.values[0].to_bits(), 0);
    }

    #[test]
    fn sorted_packed_coordinates_with_normalized_values_keep_the_given_values_without_a_copy() {
        let axis = DimensionGlobalId(ulid::Ulid(1));
        let packing = CoordinatePacking::new(&[8]).expect("3 bits fit in a packed coordinate");
        let values: ScalarBuffer<f64> = vec![1.0, 2.0].into();

        let columns = Columns::from_packed(vec![axis], packing, vec![0, 1], values.clone());

        assert!(columns.values.ptr_eq(&values));
    }

    #[test]
    fn adding_terms_on_the_same_coordinates_keeps_their_packed_coordinates() {
        let fixture = datagen::cases::Fixture::default();
        let dimensions: Vec<&Dimension> = fixture.dimensions.iter().collect();
        let axis = dimensions[1].global_id();
        let packing = CoordinatePacking::new(&[3]).expect("2 bits fit in a packed coordinate");
        let sales = Columns::from_packed(
            vec![axis],
            packing.clone(),
            vec![0, 1, 2],
            vec![3.0, 4.0, 5.0],
        );
        let cost = Columns {
            axes: sales.axes.clone(),
            packing: sales.packing.clone(),
            packed_coordinates: sales.packed_coordinates.clone(),
            values: vec![1.0, 1.0, 1.0].into(),
        };
        let packed_coordinates = sales.packed_coordinates.clone();

        let profit = Columns::add(vec![(Sign::Plus, sales), (Sign::Minus, cost)], &dimensions)
            .expect("both terms have every axis");

        assert!(profit.packed_coordinates.ptr_eq(&packed_coordinates));
        assert_eq!(&profit.values[..], &[2.0, 3.0, 4.0]);
    }
}
