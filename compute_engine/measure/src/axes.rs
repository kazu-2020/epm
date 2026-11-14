use std::collections::HashSet;
use std::sync::Arc;

use dimension::{Dimension, DimensionGlobalId, MemberIndex};

use crate::definition::MeasureDefinition;
use crate::error::{CoordinateError, MeasureError};
use crate::packing::CoordinatePacking;
use crate::scan::first_row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Axes {
    // Not the declared order: every measure packs in packing order, so the engine reads a measure without packing it again (ADR 0005).
    pub(crate) dimensions: Vec<DimensionGlobalId>,
    pub(crate) member_counts: Vec<u32>,
    pub(crate) positions: Vec<usize>,
    pub(crate) packing: CoordinatePacking,
}

impl Axes {
    pub(crate) fn new(
        definition: &MeasureDefinition,
        dimensions: &[&Dimension],
    ) -> Result<Arc<Axes>, MeasureError> {
        if definition.axes.is_empty() {
            return Err(MeasureError::NoAxes);
        }
        let mut seen = HashSet::new();
        let mut sorted = Vec::with_capacity(definition.axes.len());
        for &axis in &definition.axes {
            if !seen.insert(axis) {
                return Err(MeasureError::DuplicateAxis(axis));
            }
            let dimension = dimensions
                .iter()
                .find(|dimension| dimension.ulid() == axis)
                .ok_or(MeasureError::MissingDimension(axis))?;
            sorted.push(dimension);
        }
        sorted.sort_by_key(|dimension| dimension.packing_order());
        let member_counts: Vec<u32> = sorted.iter().map(|dimension| dimension.len()).collect();
        Ok(Arc::new(Axes {
            packing: CoordinatePacking::new(&member_counts).ok_or(MeasureError::PackingTooWide)?,
            member_counts,
            positions: definition
                .axes
                .iter()
                .map(|&axis| {
                    sorted
                        .iter()
                        .position(|dimension| dimension.ulid() == axis)
                        .expect("every declared axis was sorted above")
                })
                .collect(),
            dimensions: sorted.iter().map(|dimension| dimension.ulid()).collect(),
        }))
    }

    pub(crate) fn pack(&self, coordinate: &[MemberIndex]) -> Result<u64, CoordinateError> {
        if coordinate.len() != self.positions.len() {
            return Err(CoordinateError::Length {
                expected: self.positions.len(),
                found: coordinate.len(),
            });
        }
        if let Some((&position, &member)) = self
            .positions
            .iter()
            .zip(coordinate)
            .find(|&(&position, member)| member.0 >= self.member_counts[position])
        {
            return Err(CoordinateError::MemberOutOfRange {
                axis: self.dimensions[position],
                member,
            });
        }
        Ok(self.pack_members(coordinate.iter().map(|member| member.0)))
    }

    pub(crate) fn pack_members(&self, members: impl IntoIterator<Item = u32>) -> u64 {
        self.positions
            .iter()
            .zip(members)
            .fold(0, |packed, (&position, member)| {
                packed | self.packing.place(position, member)
            })
    }

    pub(crate) fn unpack(&self, packed: u64) -> impl Iterator<Item = u32> + '_ {
        self.positions
            .iter()
            .map(move |&position| self.packing.member(packed, position))
    }

    pub(crate) fn check_in_range(&self, packed_coordinates: &[u64]) -> Result<(), MeasureError> {
        if let Some(row) = first_row(packed_coordinates, |&packed| !self.packing.holds(packed)) {
            return Err(MeasureError::BitsOutsidePacking(row));
        }
        for (position, &member_count) in self.member_counts.iter().enumerate() {
            if let Some(row) = first_row(packed_coordinates, |&packed| {
                self.packing.member(packed, position) >= member_count
            }) {
                return Err(CoordinateError::MemberOutOfRange {
                    axis: self.dimensions[position],
                    member: MemberIndex(self.packing.member(packed_coordinates[row], position)),
                }
                .into());
            }
        }
        Ok(())
    }
}
