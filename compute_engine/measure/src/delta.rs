use std::sync::Arc;

use dimension::MemberIndex;

use crate::axes::Axes;
use crate::definition::MeasureGlobalId;
use crate::error::MeasureError;
use crate::normalize::normalize;
use crate::scan::first_unsorted;

#[derive(Debug, Clone)]
pub struct Delta {
    pub(crate) measure: MeasureGlobalId,
    pub(crate) axes: Arc<Axes>,
    // Not a hash table on coordinates: sorted packed coordinates merge with the measure's packed coordinates in one pass, with no allocation for each change.
    pub(crate) packed_coordinates: Vec<u64>,
    pub(crate) changes: Vec<Option<f64>>,
}

impl Delta {
    pub fn set(&mut self, coordinate: &[MemberIndex], value: f64) -> Result<(), MeasureError> {
        self.write(coordinate, normalize(value))
    }

    pub fn delete(&mut self, coordinate: &[MemberIndex]) -> Result<(), MeasureError> {
        self.write(coordinate, None)
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
        let packed = self.axes.pack(coordinate)?;
        self.packed_coordinates.push(packed);
        self.changes.push(change);
        Ok(())
    }

    pub(crate) fn settle(&mut self) {
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
