use std::sync::Arc;

use dimension::MemberIndex;

use crate::axes::Axes;
use crate::definition::MeasureGlobalId;
use crate::error::CoordinateError;
use crate::normalize::normalize;
use crate::scan::first_unsorted;

#[derive(Debug)]
pub struct Delta {
    pub(crate) measure: MeasureGlobalId,
    pub(crate) axes: Arc<Axes>,
    // Not a hash table on coordinates: sorted packed coordinates merge with the measure's packed coordinates in one pass, with no allocation for each change.
    pub(crate) packed_coordinates: Vec<u64>,
    pub(crate) changes: Vec<Option<f64>>,
}

impl Delta {
    pub fn packed_coordinates(&self) -> &[u64] {
        &self.packed_coordinates
    }
}

#[derive(Debug, Clone)]
pub struct DeltaBuilder {
    pub(crate) measure: MeasureGlobalId,
    pub(crate) axes: Arc<Axes>,
    pub(crate) packed_coordinates: Vec<u64>,
    pub(crate) changes: Vec<Option<f64>>,
}

impl DeltaBuilder {
    pub fn set(&mut self, coordinate: &[MemberIndex], value: f64) -> Result<(), CoordinateError> {
        self.write(coordinate, normalize(value))
    }

    pub fn delete(&mut self, coordinate: &[MemberIndex]) -> Result<(), CoordinateError> {
        self.write(coordinate, None)
    }

    // Not a sorted insert: a paste of a million cells in random order took 97 seconds.
    fn write(
        &mut self,
        coordinate: &[MemberIndex],
        change: Option<f64>,
    ) -> Result<(), CoordinateError> {
        let packed = self.axes.pack(coordinate)?;
        self.packed_coordinates.push(packed);
        self.changes.push(change);
        Ok(())
    }

    pub fn finish(self) -> Delta {
        let DeltaBuilder {
            measure,
            axes,
            mut packed_coordinates,
            mut changes,
        } = self;
        if first_unsorted(&packed_coordinates).is_some() {
            let mut writes: Vec<(u64, Option<f64>)> = packed_coordinates
                .drain(..)
                .zip(changes.drain(..))
                .collect();
            writes.sort_by_key(|&(packed, _)| packed);
            for (packed, change) in writes {
                match changes.last_mut() {
                    Some(last) if packed_coordinates.last() == Some(&packed) => *last = change,
                    _ => {
                        packed_coordinates.push(packed);
                        changes.push(change);
                    }
                }
            }
        }
        Delta {
            measure,
            axes,
            packed_coordinates,
            changes,
        }
    }
}
