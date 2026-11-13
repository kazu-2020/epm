use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch, UInt32Array};
use arrow_buffer::ScalarBuffer;
use dimension::{Dimension, DimensionGlobalId, MemberIndex};

use crate::axes::Axes;
use crate::definition::{MeasureDefinition, MeasureGlobalId};
use crate::delta::Delta;
use crate::error::MeasureError;
use crate::normalize::normalize;
use crate::packing::CoordinatePacking;
use crate::scan::{check_sorted, first_row, first_unsorted};

#[derive(Debug, Clone)]
pub struct Measure {
    definition: MeasureDefinition,
    // Not Axes itself: every version of the measure and every delta would copy it.
    axes: Arc<Axes>,
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
        let axes = Axes::new(&definition, dimensions)?;
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
        let columns: Vec<&[u32]> = base.columns()[..definition.axes.len()]
            .iter()
            .map(|column| &column.as_primitive::<UInt32Type>().values()[..])
            .collect();
        for (&position, column) in axes.positions.iter().zip(&columns) {
            let member_count = axes.member_counts[position];
            if let Some(&member) = column.iter().find(|&&member| member >= member_count) {
                return Err(MeasureError::MemberOutOfRange {
                    axis: axes.dimensions[position],
                    member: MemberIndex(member),
                });
            }
        }
        let packed_coordinates = (0..base.num_rows())
            .map(|row| axes.pack_members(columns.iter().map(|column| column[row])))
            .collect();
        let values = base.columns()[definition.axes.len()]
            .as_primitive::<Float64Type>()
            .values();
        // Not the given buffer when it is part of a larger one: an IPC file read as one buffer would keep its member columns alive.
        let values = if values.inner().capacity() > values.inner().len() {
            values.to_vec().into()
        } else {
            values.clone()
        };
        Self::checked(definition, axes, packed_coordinates, values)
    }

    pub fn try_from_packed(
        definition: MeasureDefinition,
        packed_coordinates: ScalarBuffer<u64>,
        values: ScalarBuffer<f64>,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
        let axes = Axes::new(&definition, dimensions)?;
        assert_eq!(
            packed_coordinates.len(),
            values.len(),
            "one value per packed coordinate"
        );
        axes.check_in_range(&packed_coordinates)?;
        Self::checked(definition, axes, packed_coordinates, values)
    }

    fn checked(
        definition: MeasureDefinition,
        axes: Arc<Axes>,
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
        let axes = Axes::new(&definition, dimensions)?;
        let mut packed_cells = cells
            .into_iter()
            .map(|(coordinate, value)| Ok((axes.pack(&coordinate)?, value)))
            .collect::<Result<Vec<(u64, f64)>, MeasureError>>()?;
        packed_cells.sort_unstable_by_key(|&(packed, _)| packed);
        let (packed_coordinates, values): (Vec<u64>, Vec<f64>) = packed_cells.into_iter().unzip();
        Self::checked(definition, axes, packed_coordinates.into(), values.into())
    }

    pub fn cells(&self) -> impl ExactSizeIterator<Item = (Vec<MemberIndex>, f64)> {
        let measure = self.compact();
        (0..measure.values.len()).map(move |row| {
            (
                measure
                    .axes
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
        &self.axes.packing
    }

    pub fn axes_in_packing_order(&self) -> &[DimensionGlobalId] {
        &self.axes.dimensions
    }

    pub fn axis_member_counts(
        &self,
    ) -> impl ExactSizeIterator<Item = (DimensionGlobalId, u32)> + '_ {
        self.axes
            .dimensions
            .iter()
            .copied()
            .zip(self.axes.member_counts.iter().copied())
    }

    pub fn base_packed_coordinates(&self) -> &ScalarBuffer<u64> {
        &self.packed_coordinates
    }

    pub fn base_values(&self) -> &ScalarBuffer<f64> {
        &self.values
    }

    pub fn to_record_batch(&self) -> RecordBatch {
        let measure = self.compact();
        let mut columns: Vec<ArrayRef> = measure
            .axes
            .positions
            .iter()
            .map(|&position| {
                Arc::new(UInt32Array::from_iter_values(
                    measure
                        .packed_coordinates
                        .iter()
                        .map(|&packed| measure.axes.packing.member(packed, position)),
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
        let packed = self.axes.pack(coordinate)?;
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
        self.axes.check_in_range(&packed_coordinates)?;
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
