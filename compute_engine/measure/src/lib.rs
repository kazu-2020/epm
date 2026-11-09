use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch, UInt32Array};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dimension::{Dimension, DimensionUlid, MemberId};
use ulid::Ulid;

const VALUE_COLUMN: &str = "value";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeasureUlid(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureDefinition {
    pub ulid: MeasureUlid,
    pub name: String,
    pub axes: Vec<DimensionUlid>,
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
pub struct KeyLayout {
    shifts: Vec<u32>,
    masks: Vec<u64>,
    width: u32,
}

impl KeyLayout {
    pub fn new(lengths: &[u32]) -> Option<KeyLayout> {
        let widths: Vec<u32> = lengths
            .iter()
            .map(|&len| u32::BITS - len.saturating_sub(1).leading_zeros())
            .collect();
        Self::of_widths(&widths)
    }

    pub fn reordered(&self, positions: &[usize]) -> KeyLayout {
        let widths: Vec<u32> = positions
            .iter()
            .map(|&position| self.masks[position].count_ones())
            .collect();
        Self::of_widths(&widths).expect("the same widths fit in a key")
    }

    fn of_widths(widths: &[u32]) -> Option<KeyLayout> {
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
        Some(KeyLayout {
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
            |key, ((&shift, &mask), member)| {
                debug_assert!(u64::from(member) <= mask);
                key | (u64::from(member) << shift)
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

    pub fn field(&self, axis: usize) -> u64 {
        self.masks[axis] << self.shifts[axis]
    }

    pub fn member(&self, key: u64, axis: usize) -> u32 {
        ((key >> self.shifts[axis]) & self.masks[axis]) as u32
    }

    pub fn holds(&self, key: u64) -> bool {
        key.checked_shr(self.width).unwrap_or(0) == 0
    }

    pub fn members(&self, key: u64) -> impl Iterator<Item = u32> + '_ {
        (0..self.len()).map(move |axis| self.member(key, axis))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Axis {
    dimension: DimensionUlid,
    len: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MeasureError {
    NoAxes,
    DuplicateAxis(DimensionUlid),
    MissingDimension(DimensionUlid),
    SchemaMismatch {
        expected: SchemaRef,
        found: SchemaRef,
    },
    NullInColumn(String),
    MemberOutOfRange {
        axis: DimensionUlid,
        member: MemberId,
    },
    KeyTooWide,
    KeyMismatch(usize),
    UnnormalizedValue(usize),
    UnsortedCoordinate(usize),
    DuplicateCoordinate(usize),
    CoordinateLength {
        expected: usize,
        found: usize,
    },
    ForeignDelta(MeasureUlid),
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
    layout: KeyLayout,
    // Not the u32 columns of the base: one packed key per row takes 8 B in place of 4 B per axis.
    keys: ScalarBuffer<u64>,
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
        let (axes, layout) = Self::axes(&definition, dimensions)?;
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
                    member: MemberId(member),
                });
            }
        }
        let keys = (0..base.num_rows())
            .map(|row| layout.pack(columns.iter().map(|column| column[row])))
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
        Self::checked(definition, axes, layout, keys, values)
    }

    pub fn try_from_keys(
        definition: MeasureDefinition,
        keys: ScalarBuffer<u64>,
        values: ScalarBuffer<f64>,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
        let (axes, layout) = Self::axes(&definition, dimensions)?;
        assert_eq!(keys.len(), values.len(), "one value per key");
        if let Some(row) = first_row(&keys, |&key| !layout.holds(key)) {
            return Err(MeasureError::KeyMismatch(row));
        }
        for (position, axis) in axes.iter().enumerate() {
            if let Some(row) = first_row(&keys, |&key| layout.member(key, position) >= axis.len) {
                return Err(MeasureError::MemberOutOfRange {
                    axis: axis.dimension,
                    member: MemberId(layout.member(keys[row], position)),
                });
            }
        }
        Self::checked(definition, axes, layout, keys, values)
    }

    fn axes(
        definition: &MeasureDefinition,
        dimensions: &[&Dimension],
    ) -> Result<(Vec<Axis>, KeyLayout), MeasureError> {
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
        let layout = KeyLayout::new(&axes.iter().map(|axis| axis.len).collect::<Vec<_>>())
            .ok_or(MeasureError::KeyTooWide)?;
        Ok((axes, layout))
    }

    fn checked(
        definition: MeasureDefinition,
        axes: Vec<Axis>,
        layout: KeyLayout,
        keys: ScalarBuffer<u64>,
        values: ScalarBuffer<f64>,
    ) -> Result<Self, MeasureError> {
        if let Some(row) = first_row(&values, |&value| {
            normalize(value).map(f64::to_bits) != Some(value.to_bits())
        }) {
            return Err(MeasureError::UnnormalizedValue(row));
        }
        if let Some(row) = first_unsorted(&keys) {
            return Err(match keys[row - 1].cmp(&keys[row]) {
                Ordering::Equal => MeasureError::DuplicateCoordinate(row),
                _ => MeasureError::UnsortedCoordinate(row),
            });
        }
        Ok(Measure {
            definition,
            axes,
            layout,
            keys,
            values,
            deltas: Vec::new(),
        })
    }

    pub fn from_cells(
        definition: MeasureDefinition,
        mut cells: Vec<(Vec<MemberId>, f64)>,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
        let width = definition.axes.len();
        if let Some((coordinate, _)) = cells
            .iter()
            .find(|(coordinate, _)| coordinate.len() != width)
        {
            return Err(MeasureError::CoordinateLength {
                expected: width,
                found: coordinate.len(),
            });
        }
        cells.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let mut columns: Vec<ArrayRef> = (0..width)
            .map(|axis| {
                Arc::new(UInt32Array::from_iter_values(
                    cells.iter().map(|(coordinate, _)| coordinate[axis].0),
                )) as ArrayRef
            })
            .collect();
        columns.push(Arc::new(Float64Array::from_iter_values(
            cells.iter().map(|&(_, value)| value),
        )));
        let base = RecordBatch::try_new(Arc::new(definition.schema()), columns)
            .expect("every coordinate has one member per axis");
        Self::try_new(definition, base, dimensions)
    }

    pub fn cells(&self) -> impl ExactSizeIterator<Item = (Vec<MemberId>, f64)> {
        let measure = self.compact();
        (0..measure.values.len()).map(move |row| measure.cell(row))
    }

    pub fn cell(&self, row: usize) -> (Vec<MemberId>, f64) {
        (
            self.layout.members(self.keys[row]).map(MemberId).collect(),
            self.values[row],
        )
    }

    pub fn definition(&self) -> &MeasureDefinition {
        &self.definition
    }

    pub fn ulid(&self) -> MeasureUlid {
        self.definition.ulid
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn layout(&self) -> &KeyLayout {
        &self.layout
    }

    pub fn keys(&self) -> &ScalarBuffer<u64> {
        &self.keys
    }

    pub fn values(&self) -> &ScalarBuffer<f64> {
        &self.values
    }

    pub fn to_record_batch(&self) -> RecordBatch {
        let mut columns: Vec<ArrayRef> = (0..self.axes.len())
            .map(|axis| {
                Arc::new(UInt32Array::from_iter_values(
                    self.keys.iter().map(|&key| self.layout.member(key, axis)),
                )) as ArrayRef
            })
            .collect();
        columns.push(Arc::new(Float64Array::new(self.values.clone(), None)));
        RecordBatch::try_new(Arc::new(self.definition.schema()), columns)
            .expect("the unpacked columns follow the measure schema")
    }

    pub fn changes(&self) -> impl Iterator<Item = (Vec<MemberId>, Option<f64>)> + '_ {
        self.deltas.iter().flat_map(|delta| delta.listed())
    }

    pub fn merged_changes(&self) -> (Vec<u64>, Vec<Option<f64>>) {
        match &self.deltas[..] {
            [] => (Vec::new(), Vec::new()),
            [delta] => (delta.keys.clone(), delta.changes.clone()),
            [first, rest @ ..] => rest.iter().fold(
                (first.keys.clone(), first.changes.clone()),
                |(keys, changes), delta| {
                    merge_changes(&keys, &changes, &delta.keys, &delta.changes)
                },
            ),
        }
    }

    pub fn delta_len(&self) -> usize {
        self.deltas.iter().map(|delta| delta.keys.len()).sum()
    }

    pub fn get(&self, coordinate: &[MemberId]) -> Result<Option<f64>, MeasureError> {
        if coordinate.len() != self.axes.len() {
            return Err(MeasureError::CoordinateLength {
                expected: self.axes.len(),
                found: coordinate.len(),
            });
        }
        if self
            .axes
            .iter()
            .zip(coordinate)
            .any(|(axis, member)| member.0 >= axis.len)
        {
            return Ok(None);
        }
        let key = self.layout.pack(coordinate.iter().map(|member| member.0));
        for delta in self.deltas.iter().rev() {
            if let Ok(change) = delta.keys.binary_search(&key) {
                return Ok(delta.changes[change]);
            }
        }
        Ok(self
            .keys
            .binary_search(&key)
            .ok()
            .map(|row| self.values[row]))
    }

    pub fn delta(&self) -> Delta {
        Delta {
            measure: self.definition.ulid,
            axes: self.axes.clone(),
            layout: self.layout.clone(),
            keys: Vec::new(),
            changes: Vec::new(),
        }
    }

    pub fn delta_from_sorted(
        &self,
        keys: Vec<u64>,
        changes: Vec<Option<f64>>,
    ) -> Result<Delta, MeasureError> {
        assert_eq!(keys.len(), changes.len(), "one change per key");
        if let Some(row) = first_unsorted(&keys) {
            return Err(match keys[row - 1].cmp(&keys[row]) {
                Ordering::Equal => MeasureError::DuplicateCoordinate(row),
                _ => MeasureError::UnsortedCoordinate(row),
            });
        }
        if let Some(row) = first_row(&keys, |&key| !self.layout.holds(key)) {
            return Err(MeasureError::KeyMismatch(row));
        }
        for (position, axis) in self.axes.iter().enumerate() {
            if let Some(row) =
                first_row(&keys, |&key| self.layout.member(key, position) >= axis.len)
            {
                return Err(MeasureError::MemberOutOfRange {
                    axis: axis.dimension,
                    member: MemberId(self.layout.member(keys[row], position)),
                });
            }
        }
        Ok(Delta {
            changes: changes
                .into_iter()
                .map(|change| change.and_then(normalize))
                .collect(),
            keys,
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
        if first_unsorted(&delta.keys).is_some() {
            Arc::make_mut(&mut delta).settle();
        }
        let mut measure = self.clone();
        if !delta.keys.is_empty() {
            measure.deltas.push(delta);
        }
        Ok(measure)
    }

    pub fn fold_deltas(&self) -> Measure {
        let (keys, changes) = self.merged_changes();
        let mut measure = self.clone();
        measure.deltas = vec![Arc::new(Delta {
            keys,
            changes,
            ..self.delta()
        })];
        measure
    }

    pub fn compact(&self) -> Measure {
        if self.deltas.is_empty() {
            return self.clone();
        }
        let (change_keys, changes) = self.merged_changes();
        let capacity = self.values.len() + changes.len();
        let mut keys = Vec::with_capacity(capacity);
        let mut values = Vec::with_capacity(capacity);
        let mut start = 0;
        for (&key, &change) in change_keys.iter().zip(&changes) {
            let (end, next) = match self.keys[start..].binary_search(&key) {
                Ok(row) => (start + row, start + row + 1),
                Err(row) => (start + row, start + row),
            };
            keys.extend_from_slice(&self.keys[start..end]);
            values.extend_from_slice(&self.values[start..end]);
            start = next;
            if let Some(value) = change {
                keys.push(key);
                values.push(value);
            }
        }
        keys.extend_from_slice(&self.keys[start..]);
        values.extend_from_slice(&self.values[start..]);
        Measure {
            definition: self.definition.clone(),
            axes: self.axes.clone(),
            layout: self.layout.clone(),
            keys: keys.into(),
            values: values.into(),
            deltas: Vec::new(),
        }
    }
}

fn merge_changes(
    old_keys: &[u64],
    old_changes: &[Option<f64>],
    new_keys: &[u64],
    new_changes: &[Option<f64>],
) -> (Vec<u64>, Vec<Option<f64>>) {
    let mut keys = Vec::with_capacity(old_keys.len() + new_keys.len());
    let mut changes = Vec::with_capacity(keys.capacity());
    let (mut old, mut new) = (0, 0);
    while old < old_keys.len() || new < new_keys.len() {
        let take_new =
            new < new_keys.len() && (old == old_keys.len() || new_keys[new] <= old_keys[old]);
        if take_new {
            if old < old_keys.len() && old_keys[old] == new_keys[new] {
                old += 1;
            }
            keys.push(new_keys[new]);
            changes.push(new_changes[new]);
            new += 1;
        } else {
            keys.push(old_keys[old]);
            changes.push(old_changes[old]);
            old += 1;
        }
    }
    (keys, changes)
}

#[derive(Debug, Clone)]
pub struct Delta {
    measure: MeasureUlid,
    axes: Vec<Axis>,
    layout: KeyLayout,
    // Not a hash table on coordinates: sorted keys merge with the measure's keys in one pass, with no allocation for each change.
    keys: Vec<u64>,
    changes: Vec<Option<f64>>,
}

impl Delta {
    pub fn set(&mut self, coordinate: &[MemberId], value: f64) -> Result<(), MeasureError> {
        self.check(coordinate)?;
        self.write(coordinate, normalize(value));
        Ok(())
    }

    pub fn delete(&mut self, coordinate: &[MemberId]) -> Result<(), MeasureError> {
        self.check(coordinate)?;
        self.write(coordinate, None);
        Ok(())
    }

    pub fn len(&mut self) -> usize {
        self.settle();
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&mut self) -> &[u64] {
        self.settle();
        &self.keys
    }

    pub fn changes(&mut self) -> impl ExactSizeIterator<Item = (Vec<MemberId>, Option<f64>)> + '_ {
        self.settle();
        self.listed()
    }

    fn listed(&self) -> impl ExactSizeIterator<Item = (Vec<MemberId>, Option<f64>)> + '_ {
        self.keys
            .iter()
            .zip(&self.changes)
            .map(|(&key, &change)| (self.layout.members(key).map(MemberId).collect(), change))
    }

    fn check(&self, coordinate: &[MemberId]) -> Result<(), MeasureError> {
        if coordinate.len() != self.axes.len() {
            return Err(MeasureError::CoordinateLength {
                expected: self.axes.len(),
                found: coordinate.len(),
            });
        }
        if let Some((axis, &member)) = self
            .axes
            .iter()
            .zip(coordinate)
            .find(|(axis, member)| member.0 >= axis.len)
        {
            return Err(MeasureError::MemberOutOfRange {
                axis: axis.dimension,
                member,
            });
        }
        Ok(())
    }

    // Not a sorted insert: a paste of a million cells in random order took 97 seconds.
    fn write(&mut self, coordinate: &[MemberId], change: Option<f64>) {
        self.keys
            .push(self.layout.pack(coordinate.iter().map(|member| member.0)));
        self.changes.push(change);
    }

    fn settle(&mut self) {
        if first_unsorted(&self.keys).is_none() {
            return;
        }
        let mut writes: Vec<(u64, Option<f64>)> =
            self.keys.drain(..).zip(self.changes.drain(..)).collect();
        writes.sort_by_key(|&(key, _)| key);
        for (key, change) in writes {
            match self.changes.last_mut() {
                Some(last) if self.keys.last() == Some(&key) => *last = change,
                _ => {
                    self.keys.push(key);
                    self.changes.push(change);
                }
            }
        }
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

pub fn first_unsorted(keys: &[u64]) -> Option<usize> {
    (1..keys.len())
        .step_by(CHECKED_ROWS)
        .collect::<Vec<usize>>()
        .into_par_iter()
        .filter_map(|start| {
            let end = (start + CHECKED_ROWS).min(keys.len());
            keys[start - 1..end]
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
        let keys: Vec<u64> = (0..=CHECKED_ROWS as u64).chain([0]).collect();

        assert_eq!(first_unsorted(&keys), Some(CHECKED_ROWS + 1));
        let last_of_the_first_piece: Vec<u64> = (0..CHECKED_ROWS as u64).chain([0]).collect();
        assert_eq!(first_unsorted(&last_of_the_first_piece), Some(CHECKED_ROWS));
        assert_eq!(
            first_row(&keys, |&key| key == CHECKED_ROWS as u64),
            Some(CHECKED_ROWS)
        );
    }
}
