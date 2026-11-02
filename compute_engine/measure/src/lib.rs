use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;
use std::hash::BuildHasher;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use dimension::{Dimension, DimensionUlid, MemberId};
use foldhash::fast::RandomState;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Axis {
    dimension: DimensionUlid,
    len: u32,
}

#[derive(Debug, PartialEq)]
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
    base: RecordBatch,
    coordinate_columns: Vec<UInt32Array>,
    values: Float64Array,
    // Not Vec<Delta>: every version would copy all earlier deltas of the measure.
    deltas: Vec<Arc<Delta>>,
}

impl Measure {
    pub fn try_new(
        definition: MeasureDefinition,
        base: RecordBatch,
        dimensions: &[&Dimension],
    ) -> Result<Self, MeasureError> {
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

        let base = RecordBatch::try_new(Arc::new(expected), base.columns().to_vec())
            .expect("columns without nulls fit the non-nullable measure schema");
        let measure = Self::from_parts(definition, axes, base);
        let (axes, coordinate_columns, values) =
            (&measure.axes, &measure.coordinate_columns, &measure.values);
        for (axis, column) in axes.iter().zip(coordinate_columns) {
            if let Some(&member) = column.values().iter().find(|&&member| member >= axis.len) {
                return Err(MeasureError::MemberOutOfRange {
                    axis: axis.dimension,
                    member: MemberId(member),
                });
            }
        }
        for (row, &value) in values.values().iter().enumerate() {
            if normalize(value).map(f64::to_bits) != Some(value.to_bits()) {
                return Err(MeasureError::UnnormalizedValue(row));
            }
        }
        for row in 1..values.len() {
            let previous = coordinate_columns
                .iter()
                .map(|column| column.value(row - 1));
            match previous.cmp(coordinate_columns.iter().map(|column| column.value(row))) {
                Ordering::Less => {}
                Ordering::Equal => return Err(MeasureError::DuplicateCoordinate(row)),
                Ordering::Greater => return Err(MeasureError::UnsortedCoordinate(row)),
            }
        }
        Ok(measure)
    }

    pub fn definition(&self) -> &MeasureDefinition {
        &self.definition
    }

    pub fn ulid(&self) -> MeasureUlid {
        self.definition.ulid
    }

    pub fn base(&self) -> &RecordBatch {
        &self.base
    }

    pub fn delta_len(&self) -> usize {
        self.deltas.iter().map(|delta| delta.len()).sum()
    }

    pub fn get(&self, coordinate: &[MemberId]) -> Result<Option<f64>, MeasureError> {
        if coordinate.len() != self.axes.len() {
            return Err(MeasureError::CoordinateLength {
                expected: self.axes.len(),
                found: coordinate.len(),
            });
        }
        let hash = hash_coordinate(coordinate);
        for delta in self.deltas.iter().rev() {
            if let Some(change) = delta.change(hash, coordinate) {
                return Ok(change);
            }
        }
        Ok(self
            .search(0, coordinate)
            .ok()
            .map(|row| self.values.value(row)))
    }

    pub fn delta(&self) -> Delta {
        Delta {
            measure: self.definition.ulid,
            axes: self.axes.clone(),
            coordinates: Vec::new(),
            changes: Vec::new(),
            index: HashTable::new(),
        }
    }

    pub fn apply(&self, delta: impl Into<Arc<Delta>>) -> Result<Measure, MeasureError> {
        let delta = delta.into();
        if delta.measure != self.definition.ulid {
            return Err(MeasureError::ForeignDelta(delta.measure));
        }
        if delta.axes != self.axes {
            return Err(MeasureError::DeltaAxesMismatch);
        }
        let mut measure = self.clone();
        if !delta.is_empty() {
            measure.deltas.push(delta);
        }
        Ok(measure)
    }

    pub fn fold_deltas(&self) -> Measure {
        let mut measure = self.clone();
        measure.deltas = vec![Arc::new(self.merged_delta())];
        measure
    }

    pub fn compact(&self) -> Measure {
        if self.deltas.is_empty() {
            return self.clone();
        }
        let merged = self.merged_delta();
        let mut order: Vec<usize> = (0..merged.changes.len()).collect();
        order.sort_unstable_by(|&left, &right| {
            merged.coordinate(left).cmp(merged.coordinate(right))
        });

        // Not `+`: a capacity hint is invisible to tests, so cargo-mutants would report its `*` mutant as surviving.
        let capacity = self.values.len().saturating_add(merged.changes.len());
        let mut coordinate_columns: Vec<Vec<u32>> = self
            .axes
            .iter()
            .map(|_| Vec::with_capacity(capacity))
            .collect();
        let mut values = Vec::with_capacity(capacity);
        let mut start = 0;
        for change in order {
            let coordinate = merged.coordinate(change);
            let (end, next) = match self.search(start, coordinate) {
                Ok(row) => (row, row + 1),
                Err(row) => (row, row),
            };
            self.copy_rows(start..end, &mut coordinate_columns, &mut values);
            start = next;
            if let Some(value) = merged.changes[change] {
                for (output, member) in coordinate_columns.iter_mut().zip(coordinate) {
                    output.push(member.0);
                }
                values.push(value);
            }
        }
        self.copy_rows(
            start..self.values.len(),
            &mut coordinate_columns,
            &mut values,
        );

        let mut columns: Vec<ArrayRef> = coordinate_columns
            .into_iter()
            .map(|column| Arc::new(UInt32Array::from(column)) as ArrayRef)
            .collect();
        columns.push(Arc::new(Float64Array::from(values)));
        let base = RecordBatch::try_new(Arc::new(self.definition.schema()), columns)
            .expect("compaction builds columns in the measure schema");
        Self::from_parts(self.definition.clone(), self.axes.clone(), base)
    }

    fn merged_delta(&self) -> Delta {
        let mut merged = self.delta();
        for delta in &self.deltas {
            for change in 0..delta.changes.len() {
                merged.write(delta.coordinate(change), delta.changes[change]);
            }
        }
        merged
    }

    fn from_parts(definition: MeasureDefinition, axes: Vec<Axis>, base: RecordBatch) -> Measure {
        let columns = base.columns();
        let coordinate_columns = columns[..axes.len()]
            .iter()
            .map(|column| column.as_primitive::<UInt32Type>().clone())
            .collect();
        let values = columns[axes.len()].as_primitive::<Float64Type>().clone();
        Measure {
            definition,
            axes,
            base,
            coordinate_columns,
            values,
            deltas: Vec::new(),
        }
    }

    fn copy_rows(
        &self,
        rows: Range<usize>,
        coordinate_columns: &mut [Vec<u32>],
        values: &mut Vec<f64>,
    ) {
        for (column, output) in self.coordinate_columns.iter().zip(coordinate_columns) {
            output.extend_from_slice(&column.values()[rows.clone()]);
        }
        values.extend_from_slice(&self.values.values()[rows]);
    }

    fn search(&self, from: usize, coordinate: &[MemberId]) -> Result<usize, usize> {
        let (mut low, mut high) = (from, self.values.len());
        while low < high {
            let middle = usize::midpoint(low, high);
            let row_coordinate = self
                .coordinate_columns
                .iter()
                .map(|column| column.value(middle));
            match row_coordinate.cmp(coordinate.iter().map(|member| member.0)) {
                Ordering::Less => low = middle + 1,
                Ordering::Equal => return Ok(middle),
                Ordering::Greater => high = middle,
            }
        }
        Err(low)
    }
}

#[derive(Debug)]
pub struct Delta {
    measure: MeasureUlid,
    axes: Vec<Axis>,
    coordinates: Vec<MemberId>,
    changes: Vec<Option<f64>>,
    // Not HashMap<Vec<MemberId>, _>: one allocation per coordinate would dominate the delta's memory.
    index: HashTable<u32>,
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

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn changes(&self) -> impl ExactSizeIterator<Item = (&[MemberId], Option<f64>)> {
        (0..self.changes.len()).map(|change| (self.coordinate(change), self.changes[change]))
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

    fn coordinate(&self, change: usize) -> &[MemberId] {
        let width = self.axes.len();
        &self.coordinates[change * width..(change + 1) * width]
    }

    fn change(&self, hash: u64, coordinate: &[MemberId]) -> Option<Option<f64>> {
        let change = self.index.find(hash, |&change| {
            self.coordinate(change as usize) == coordinate
        })?;
        Some(self.changes[*change as usize])
    }

    fn write(&mut self, coordinate: &[MemberId], change: Option<f64>) {
        let width = self.axes.len();
        let coordinates = &self.coordinates;
        let coordinate_at =
            |change: u32| &coordinates[change as usize * width..(change as usize + 1) * width];
        match self.index.entry(
            hash_coordinate(coordinate),
            |&other| coordinate_at(other) == coordinate,
            |&other| hash_coordinate(coordinate_at(other)),
        ) {
            Entry::Occupied(slot) => self.changes[*slot.get() as usize] = change,
            Entry::Vacant(slot) => {
                let position = u32::try_from(self.changes.len())
                    .expect("a delta holds fewer than 2^32 changes");
                slot.insert(position);
                self.coordinates.extend_from_slice(coordinate);
                self.changes.push(change);
            }
        }
    }
}

// Not a per-delta RandomState: every layer must agree on a coordinate's hash so a read hashes it once.
// Not FixedState: foldhash and this repository are public, so a fixed seed lets anyone search colliding coordinates offline.
static COORDINATE_HASHER: LazyLock<RandomState> = LazyLock::new(RandomState::default);

fn hash_coordinate(coordinate: &[MemberId]) -> u64 {
    COORDINATE_HASHER.hash_one(coordinate)
}
