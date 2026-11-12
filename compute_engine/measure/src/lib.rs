use rayon::prelude::*;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch, UInt32Array};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dimension::{Dimension, DimensionGlobalId, MemberIndex};
use ulid::Ulid;

const VALUE_COLUMN: &str = "value";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeasureGlobalId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureDefinition {
    pub ulid: MeasureGlobalId,
    pub name: String,
    pub axes: Vec<DimensionGlobalId>,
}

impl MeasureDefinition {
    pub fn schema(&self) -> Schema {
        let mut fields: Vec<Field> = self
            .axes
            .iter()
            .map(|axis| Field::new(axis.0.to_string(), DataType::UInt32, false))
            .collect();
        fields.push(Field::new(VALUE_COLUMN, DataType::Float64, false));
        Schema::new(fields)
    }
}

pub fn normalize(value: f64) -> Option<f64> {
    if !value.is_finite() {
        None
    } else if value == 0.0 {
        Some(0.0)
    } else {
        Some(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatePacking {
    shifts: Vec<u32>,
    masks: Vec<u64>,
    width: u32,
}

impl CoordinatePacking {
    pub fn new(lengths: &[u32]) -> Option<CoordinatePacking> {
        let widths: Vec<u32> = lengths
            .iter()
            .map(|&len| u32::BITS - len.saturating_sub(1).leading_zeros())
            .collect();
        Self::of_widths(&widths)
    }

    pub fn reordered(&self, positions: &[usize]) -> CoordinatePacking {
        let widths: Vec<u32> = positions
            .iter()
            .map(|&position| self.masks[position].count_ones())
            .collect();
        Self::of_widths(&widths).expect("the same widths fit in a packed coordinate")
    }

    fn of_widths(widths: &[u32]) -> Option<CoordinatePacking> {
        let width = widths.iter().sum::<u32>();
        if width > u64::BITS {
            return None;
        }
        let mut shifts = vec![0; widths.len()];
        let mut shift = 0;
        for (slot, &width) in shifts.iter_mut().zip(widths).rev() {
            // Not 64 for a zero-width axis in front of 64 bits: a u64 shifted by 64 overflows.
            *slot = shift.min(u64::BITS - 1);
            shift += width;
        }
        Some(CoordinatePacking {
            shifts,
            masks: widths.iter().map(|&width| (1u64 << width) - 1).collect(),
            width,
        })
    }

    pub fn len(&self) -> usize {
        self.shifts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shifts.is_empty()
    }

    pub fn pack(&self, members: impl IntoIterator<Item = u32>) -> u64 {
        self.shifts.iter().zip(&self.masks).zip(members).fold(
            0,
            |packed, ((&shift, &mask), member)| {
                debug_assert!(u64::from(member) <= mask);
                packed | (u64::from(member) << shift)
            },
        )
    }

    pub fn place(&self, axis: usize, member: u32) -> u64 {
        debug_assert!(self.fits(axis, member));
        u64::from(member) << self.shifts[axis]
    }

    pub fn fits(&self, axis: usize, member: u32) -> bool {
        u64::from(member) <= self.masks[axis]
    }

    pub fn axis_mask(&self, axis: usize) -> u64 {
        self.masks[axis] << self.shifts[axis]
    }

    pub fn member(&self, packed: u64, axis: usize) -> u32 {
        ((packed >> self.shifts[axis]) & self.masks[axis]) as u32
    }

    pub fn holds(&self, packed: u64) -> bool {
        packed.checked_shr(self.width).unwrap_or(0) == 0
    }

    pub fn unpack(&self, packed: u64) -> impl Iterator<Item = u32> + '_ {
        (0..self.len()).map(move |axis| self.member(packed, axis))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Axis {
    dimension: DimensionGlobalId,
    len: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MeasureError {
    NoAxes,
    DuplicateAxis(DimensionGlobalId),
    MissingDimension(DimensionGlobalId),
    SchemaMismatch {
        expected: SchemaRef,
        found: SchemaRef,
    },
    NullInColumn(String),
    MemberOutOfRange {
        axis: DimensionGlobalId,
        member: MemberIndex,
    },
    PackingTooWide,
    BitsOutsidePacking(usize),
    UnnormalizedValue(usize),
    UnsortedCoordinate(usize),
    DuplicateCoordinate(usize),
    CoordinateLength {
        expected: usize,
        found: usize,
    },
    ForeignDelta(MeasureGlobalId),
    DeltaAxesMismatch,
}

impl fmt::Display for MeasureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for MeasureError {}

#[derive(Debug, Clone)]
pub struct Measure {
    definition: MeasureDefinition,
    axes: Vec<Axis>,
    packing: CoordinatePacking,
    // Not the u32 columns of the base: one packed coordinate per row takes 8 B in place of 4 B per axis.
    packed_coordinates: ScalarBuffer<u64>,
    values: ScalarBuffer<f64>,
    // Not Vec<Delta>: every version would copy all earlier deltas of the measure.
    deltas: Vec<Arc<Delta>>,
}

impl Measure {
    pub fn try_new(
        definition: MeasureDefinition,
        base: RecordBatch,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
        let (axes, packing) = Self::axes(&definition, dimensions)?;
        let expected = definition.schema();
        let found = base.schema();
        let same_shape =
            expected.fields().len() == found.fields().len()
                && expected.fields().iter().zip(found.fields()).all(
                    |(expected_field, found_field)| {
                        expected_field.name() == found_field.name()
                            && expected_field.data_type() == found_field.data_type()
                    },
                );
        if !same_shape {
            return Err(MeasureError::SchemaMismatch {
                expected: Arc::new(expected),
                found,
            });
        }
        if let Some((field, _)) = found
            .fields()
            .iter()
            .zip(base.columns())
            .find(|(_, column)| column.null_count() > 0)
        {
            return Err(MeasureError::NullInColumn(field.name().clone()));
        }
        let columns: Vec<&[u32]> = base.columns()[..axes.len()]
            .iter()
            .map(|column| &column.as_primitive::<UInt32Type>().values()[..])
            .collect();
        for (axis, column) in axes.iter().zip(&columns) {
            if let Some(&member) = column.iter().find(|&&member| member >= axis.len) {
                return Err(MeasureError::MemberOutOfRange {
                    axis: axis.dimension,
                    member: MemberIndex(member),
                });
            }
        }
        let packed_coordinates = (0..base.num_rows())
            .map(|row| packing.pack(columns.iter().map(|column| column[row])))
            .collect();
        let values = base.columns()[axes.len()]
            .as_primitive::<Float64Type>()
            .values();
        // Not the given buffer when it is part of a larger one: an IPC file read as one buffer would keep its member columns alive.
        let values = if values.inner().capacity() > values.inner().len() {
            values.to_vec().into()
        } else {
            values.clone()
        };
        Self::checked(definition, axes, packing, packed_coordinates, values)
    }

    pub fn try_from_packed(
        definition: MeasureDefinition,
        packed_coordinates: ScalarBuffer<u64>,
        values: ScalarBuffer<f64>,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
        let (axes, packing) = Self::axes(&definition, dimensions)?;
        assert_eq!(
            packed_coordinates.len(),
            values.len(),
            "one value per packed coordinate"
        );
        check_in_range(&axes, &packing, &packed_coordinates)?;
        Self::checked(definition, axes, packing, packed_coordinates, values)
    }

    fn axes(
        definition: &MeasureDefinition,
        dimensions: &[&Dimension],
    ) -> Result<(Vec<Axis>, CoordinatePacking), MeasureError> {
        if definition.axes.is_empty() {
            return Err(MeasureError::NoAxes);
        }
        let mut seen = HashSet::new();
        let mut axes = Vec::with_capacity(definition.axes.len());
        for &axis in &definition.axes {
            if !seen.insert(axis) {
                return Err(MeasureError::DuplicateAxis(axis));
            }
            let dimension = dimensions
                .iter()
                .find(|dimension| dimension.ulid() == axis)
                .ok_or(MeasureError::MissingDimension(axis))?;
            axes.push(Axis {
                dimension: axis,
                len: dimension.len(),
            });
        }
        let packing = CoordinatePacking::new(&axes.iter().map(|axis| axis.len).collect::<Vec<_>>())
            .ok_or(MeasureError::PackingTooWide)?;
        Ok((axes, packing))
    }

    fn checked(
        definition: MeasureDefinition,
        axes: Vec<Axis>,
        packing: CoordinatePacking,
        packed_coordinates: ScalarBuffer<u64>,
        values: ScalarBuffer<f64>,
    ) -> Result<Self, MeasureError> {
        if let Some(row) = first_row(&values, |&value| {
            normalize(value).map(f64::to_bits) != Some(value.to_bits())
        }) {
            return Err(MeasureError::UnnormalizedValue(row));
        }
        check_sorted(&packed_coordinates)?;
        Ok(Measure {
            definition,
            axes,
            packing,
            packed_coordinates,
            values,
            deltas: Vec::new(),
        })
    }

    pub fn from_cells(
        definition: MeasureDefinition,
        cells: Vec<(Vec<MemberIndex>, f64)>,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
        let (axes, packing) = Self::axes(&definition, dimensions)?;
        let mut packed_cells = cells
            .into_iter()
            .map(|(coordinate, value)| Ok((pack_checked(&axes, &packing, &coordinate)?, value)))
            .collect::<Result<Vec<(u64, f64)>, MeasureError>>()?;
        packed_cells.sort_unstable_by_key(|&(packed, _)| packed);
        let (packed_coordinates, values): (Vec<u64>, Vec<f64>) = packed_cells.into_iter().unzip();
        Self::checked(
            definition,
            axes,
            packing,
            packed_coordinates.into(),
            values.into(),
        )
    }

    pub fn cells(&self) -> impl ExactSizeIterator<Item = (Vec<MemberIndex>, f64)> {
        let measure = self.compact();
        (0..measure.values.len()).map(move |row| {
            (
                measure
                    .packing
                    .unpack(measure.packed_coordinates[row])
                    .map(MemberIndex)
                    .collect(),
                measure.values[row],
            )
        })
    }

    pub fn definition(&self) -> &MeasureDefinition {
        &self.definition
    }

    pub fn ulid(&self) -> MeasureGlobalId {
        self.definition.ulid
    }

    pub fn packing(&self) -> &CoordinatePacking {
        &self.packing
    }

    pub fn axis_lengths(&self) -> impl ExactSizeIterator<Item = (DimensionGlobalId, u32)> + '_ {
        self.axes.iter().map(|axis| (axis.dimension, axis.len))
    }

    pub fn base_packed_coordinates(&self) -> &ScalarBuffer<u64> {
        &self.packed_coordinates
    }

    pub fn base_values(&self) -> &ScalarBuffer<f64> {
        &self.values
    }

    pub fn to_record_batch(&self) -> RecordBatch {
        let measure = self.compact();
        let mut columns: Vec<ArrayRef> = (0..measure.axes.len())
            .map(|axis| {
                Arc::new(UInt32Array::from_iter_values(
                    measure
                        .packed_coordinates
                        .iter()
                        .map(|&packed| measure.packing.member(packed, axis)),
                )) as ArrayRef
            })
            .collect();
        columns.push(Arc::new(Float64Array::new(measure.values.clone(), None)));
        RecordBatch::try_new(Arc::new(measure.definition.schema()), columns)
            .expect("the unpacked columns follow the measure schema")
    }

    pub fn merged_changes(&self) -> (Vec<u64>, Vec<Option<f64>>) {
        match &self.deltas[..] {
            [] => (Vec::new(), Vec::new()),
            [delta] => (delta.packed_coordinates.clone(), delta.changes.clone()),
            [first, rest @ ..] => rest.iter().fold(
                (first.packed_coordinates.clone(), first.changes.clone()),
                |(packed_coordinates, changes), delta| {
                    merge_changes(
                        &packed_coordinates,
                        &changes,
                        &delta.packed_coordinates,
                        &delta.changes,
                    )
                },
            ),
        }
    }

    pub fn delta_len(&self) -> usize {
        self.deltas
            .iter()
            .map(|delta| delta.packed_coordinates.len())
            .sum()
    }

    pub fn get(&self, coordinate: &[MemberIndex]) -> Result<Option<f64>, MeasureError> {
        let packed = pack_checked(&self.axes, &self.packing, coordinate)?;
        for delta in self.deltas.iter().rev() {
            if let Ok(change) = delta.packed_coordinates.binary_search(&packed) {
                return Ok(delta.changes[change]);
            }
        }
        Ok(self
            .packed_coordinates
            .binary_search(&packed)
            .ok()
            .map(|row| self.values[row]))
    }

    pub fn delta(&self) -> Delta {
        Delta {
            measure: self.definition.ulid,
            axes: self.axes.clone(),
            packing: self.packing.clone(),
            packed_coordinates: Vec::new(),
            changes: Vec::new(),
        }
    }

    pub fn delta_from_packed(
        &self,
        packed_coordinates: Vec<u64>,
        changes: Vec<Option<f64>>,
    ) -> Result<Delta, MeasureError> {
        assert_eq!(
            packed_coordinates.len(),
            changes.len(),
            "one change per packed coordinate"
        );
        check_sorted(&packed_coordinates)?;
        check_in_range(&self.axes, &self.packing, &packed_coordinates)?;
        Ok(Delta {
            changes: changes
                .into_iter()
                .map(|change| change.and_then(normalize))
                .collect(),
            packed_coordinates,
            ..self.delta()
        })
    }

    pub fn apply(&self, delta: impl Into<Arc<Delta>>) -> Result<Measure, MeasureError> {
        let mut delta = delta.into();
        if delta.measure != self.definition.ulid {
            return Err(MeasureError::ForeignDelta(delta.measure));
        }
        if delta.axes != self.axes {
            return Err(MeasureError::DeltaAxesMismatch);
        }
        if first_unsorted(&delta.packed_coordinates).is_some() {
            Arc::make_mut(&mut delta).settle();
        }
        let mut measure = self.clone();
        if !delta.packed_coordinates.is_empty() {
            measure.deltas.push(delta);
        }
        Ok(measure)
    }

    pub fn fold_deltas(&self) -> Measure {
        if matches!(self.deltas[..], [] | [_]) {
            return self.clone();
        }
        let (packed_coordinates, changes) = self.merged_changes();
        let mut measure = self.clone();
        measure.deltas = vec![Arc::new(Delta {
            packed_coordinates,
            changes,
            ..self.delta()
        })];
        measure
    }

    pub fn compact(&self) -> Measure {
        if self.deltas.is_empty() {
            return self.clone();
        }
        let (change_packed_coordinates, changes) = self.merged_changes();
        let capacity = self.values.len() + changes.len();
        let mut packed_coordinates = Vec::with_capacity(capacity);
        let mut values = Vec::with_capacity(capacity);
        let mut start = 0;
        for (&packed, &change) in change_packed_coordinates.iter().zip(&changes) {
            let (end, next) = match self.packed_coordinates[start..].binary_search(&packed) {
                Ok(row) => (start + row, start + row + 1),
                Err(row) => (start + row, start + row),
            };
            packed_coordinates.extend_from_slice(&self.packed_coordinates[start..end]);
            values.extend_from_slice(&self.values[start..end]);
            start = next;
            if let Some(value) = change {
                packed_coordinates.push(packed);
                values.push(value);
            }
        }
        packed_coordinates.extend_from_slice(&self.packed_coordinates[start..]);
        values.extend_from_slice(&self.values[start..]);
        Measure {
            definition: self.definition.clone(),
            axes: self.axes.clone(),
            packing: self.packing.clone(),
            packed_coordinates: packed_coordinates.into(),
            values: values.into(),
            deltas: Vec::new(),
        }
    }
}

fn merge_changes(
    old_packed_coordinates: &[u64],
    old_changes: &[Option<f64>],
    new_packed_coordinates: &[u64],
    new_changes: &[Option<f64>],
) -> (Vec<u64>, Vec<Option<f64>>) {
    let mut packed_coordinates =
        Vec::with_capacity(old_packed_coordinates.len() + new_packed_coordinates.len());
    let mut changes = Vec::with_capacity(packed_coordinates.capacity());
    let (mut old, mut new) = (0, 0);
    while old < old_packed_coordinates.len() || new < new_packed_coordinates.len() {
        let take_new = new < new_packed_coordinates.len()
            && (old == old_packed_coordinates.len()
                || new_packed_coordinates[new] <= old_packed_coordinates[old]);
        if take_new {
            if old < old_packed_coordinates.len()
                && old_packed_coordinates[old] == new_packed_coordinates[new]
            {
                old += 1;
            }
            packed_coordinates.push(new_packed_coordinates[new]);
            changes.push(new_changes[new]);
            new += 1;
        } else {
            packed_coordinates.push(old_packed_coordinates[old]);
            changes.push(old_changes[old]);
            old += 1;
        }
    }
    (packed_coordinates, changes)
}

#[derive(Debug, Clone)]
pub struct Delta {
    measure: MeasureGlobalId,
    axes: Vec<Axis>,
    packing: CoordinatePacking,
    // Not a hash table on coordinates: sorted packed coordinates merge with the measure's packed coordinates in one pass, with no allocation for each change.
    packed_coordinates: Vec<u64>,
    changes: Vec<Option<f64>>,
}

impl Delta {
    pub fn set(&mut self, coordinate: &[MemberIndex], value: f64) -> Result<(), MeasureError> {
        self.write(coordinate, normalize(value))
    }

    pub fn delete(&mut self, coordinate: &[MemberIndex]) -> Result<(), MeasureError> {
        self.write(coordinate, None)
    }

    pub fn is_empty(&self) -> bool {
        self.packed_coordinates.is_empty()
    }

    pub fn packed_coordinates(&mut self) -> &[u64] {
        self.settle();
        &self.packed_coordinates
    }

    // Not a sorted insert: a paste of a million cells in random order took 97 seconds.
    fn write(
        &mut self,
        coordinate: &[MemberIndex],
        change: Option<f64>,
    ) -> Result<(), MeasureError> {
        let packed = pack_checked(&self.axes, &self.packing, coordinate)?;
        self.packed_coordinates.push(packed);
        self.changes.push(change);
        Ok(())
    }

    fn settle(&mut self) {
        if first_unsorted(&self.packed_coordinates).is_none() {
            return;
        }
        let mut writes: Vec<(u64, Option<f64>)> = self
            .packed_coordinates
            .drain(..)
            .zip(self.changes.drain(..))
            .collect();
        writes.sort_by_key(|&(packed, _)| packed);
        for (packed, change) in writes {
            match self.changes.last_mut() {
                Some(last) if self.packed_coordinates.last() == Some(&packed) => *last = change,
                _ => {
                    self.packed_coordinates.push(packed);
                    self.changes.push(change);
                }
            }
        }
    }
}

fn pack_checked(
    axes: &[Axis],
    packing: &CoordinatePacking,
    coordinate: &[MemberIndex],
) -> Result<u64, MeasureError> {
    if coordinate.len() != axes.len() {
        return Err(MeasureError::CoordinateLength {
            expected: axes.len(),
            found: coordinate.len(),
        });
    }
    if let Some((axis, &member)) = axes
        .iter()
        .zip(coordinate)
        .find(|(axis, member)| member.0 >= axis.len)
    {
        return Err(MeasureError::MemberOutOfRange {
            axis: axis.dimension,
            member,
        });
    }
    Ok(packing.pack(coordinate.iter().map(|member| member.0)))
}

fn check_in_range(
    axes: &[Axis],
    packing: &CoordinatePacking,
    packed_coordinates: &[u64],
) -> Result<(), MeasureError> {
    if let Some(row) = first_row(packed_coordinates, |&packed| !packing.holds(packed)) {
        return Err(MeasureError::BitsOutsidePacking(row));
    }
    for (position, axis) in axes.iter().enumerate() {
        if let Some(row) = first_row(packed_coordinates, |&packed| {
            packing.member(packed, position) >= axis.len
        }) {
            return Err(MeasureError::MemberOutOfRange {
                axis: axis.dimension,
                member: MemberIndex(packing.member(packed_coordinates[row], position)),
            });
        }
    }
    Ok(())
}

fn check_sorted(packed_coordinates: &[u64]) -> Result<(), MeasureError> {
    match first_unsorted(packed_coordinates) {
        None => Ok(()),
        Some(row) if packed_coordinates[row - 1] == packed_coordinates[row] => {
            Err(MeasureError::DuplicateCoordinate(row))
        }
        Some(row) => Err(MeasureError::UnsortedCoordinate(row)),
    }
}

const CHECKED_ROWS: usize = 1 << 16;

pub fn first_row<T: Sync>(items: &[T], bad: impl Fn(&T) -> bool + Sync) -> Option<usize> {
    items
        .par_chunks(CHECKED_ROWS)
        .enumerate()
        .filter_map(|(piece, chunk)| {
            chunk
                .iter()
                .position(&bad)
                .map(|row| piece * CHECKED_ROWS + row)
        })
        .min()
}

pub fn first_unsorted(packed_coordinates: &[u64]) -> Option<usize> {
    (1..packed_coordinates.len())
        .step_by(CHECKED_ROWS)
        .collect::<Vec<usize>>()
        .into_par_iter()
        .filter_map(|start| {
            let end = (start + CHECKED_ROWS).min(packed_coordinates.len());
            packed_coordinates[start - 1..end]
                .windows(2)
                .position(|pair| pair[0] >= pair[1])
                .map(|row| start + row)
        })
        .min()
}

#[cfg(test)]
mod parallel_check_tests {
    use super::*;

    #[test]
    fn the_first_bad_row_is_found_across_the_border_of_two_pieces() {
        let packed_coordinates: Vec<u64> = (0..=CHECKED_ROWS as u64).chain([0]).collect();

        assert_eq!(first_unsorted(&packed_coordinates), Some(CHECKED_ROWS + 1));
        let last_of_the_first_piece: Vec<u64> = (0..CHECKED_ROWS as u64).chain([0]).collect();
        assert_eq!(first_unsorted(&last_of_the_first_piece), Some(CHECKED_ROWS));
        assert_eq!(
            first_row(&packed_coordinates, |&packed| packed == CHECKED_ROWS as u64),
            Some(CHECKED_ROWS)
        );
    }

    #[test]
    fn short_inputs_have_no_bad_row_unless_one_is_there() {
        assert_eq!(first_unsorted(&[]), None);
        assert_eq!(first_unsorted(&[7]), None);
        assert_eq!(first_unsorted(&[7, 7]), Some(1));
        assert_eq!(first_row(&[] as &[u64], |_| true), None);
        assert_eq!(first_row(&[7u64], |&packed| packed == 7), Some(0));
    }

    #[test]
    fn a_bad_row_at_the_end_of_two_full_pieces_is_found() {
        let sorted: Vec<u64> = (0..2 * CHECKED_ROWS as u64).collect();
        assert_eq!(first_unsorted(&sorted), None);

        let mut last_repeated = sorted.clone();
        last_repeated[2 * CHECKED_ROWS - 1] = last_repeated[2 * CHECKED_ROWS - 2];
        assert_eq!(first_unsorted(&last_repeated), Some(2 * CHECKED_ROWS - 1));
        assert_eq!(
            first_row(&sorted, |&packed| packed + 1 == 2 * CHECKED_ROWS as u64),
            Some(2 * CHECKED_ROWS - 1)
        );
    }
}
