use std::collections::{BTreeMap, BTreeSet};

use dimension::{DimensionGlobalId, MemberIndex};
use measure::CoordinatePacking;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Region {
    members: BTreeMap<DimensionGlobalId, BTreeSet<u32>>,
}

impl Region {
    pub fn of_changes<'c>(
        axes: &[DimensionGlobalId],
        coordinates: impl IntoIterator<Item = &'c [MemberIndex]>,
    ) -> Option<Region> {
        let mut region = Region {
            members: axes.iter().map(|&axis| (axis, BTreeSet::new())).collect(),
        };
        for coordinate in coordinates {
            for (axis, member) in axes.iter().zip(coordinate) {
                region
                    .members
                    .get_mut(axis)
                    .expect("every axis has a set")
                    .insert(member.0);
            }
        }
        region.non_empty()
    }

    pub(crate) fn of_packed(
        axes: &[DimensionGlobalId],
        packing: &CoordinatePacking,
        packed_coordinates: &[u64],
    ) -> Option<Region> {
        // Not BTreeSet::insert for each packed coordinate: a change of hundreds of thousands of cells paid a tree insert per axis per cell.
        let mut seen: Vec<Vec<bool>> = vec![Vec::new(); axes.len()];
        for &packed in packed_coordinates {
            for (axis, members) in seen.iter_mut().enumerate() {
                let member = packing.member(packed, axis) as usize;
                if member >= members.len() {
                    members.resize(member + 1, false);
                }
                members[member] = true;
            }
        }
        Region {
            members: axes
                .iter()
                .zip(seen)
                .map(|(&axis, members)| {
                    let members = (0..members.len() as u32)
                        .filter(|&member| members[member as usize])
                        .collect();
                    (axis, members)
                })
                .collect(),
        }
        .non_empty()
    }

    pub fn members(&self, dimension: DimensionGlobalId) -> Option<&BTreeSet<u32>> {
        self.members.get(&dimension)
    }

    pub(crate) fn constrain(&mut self, dimension: DimensionGlobalId, members: BTreeSet<u32>) {
        let members = match self.members.get(&dimension) {
            Some(kept) => kept.intersection(&members).copied().collect(),
            None => members,
        };
        self.members.insert(dimension, members);
    }

    pub(crate) fn without(mut self, dimension: DimensionGlobalId) -> Region {
        self.members.remove(&dimension);
        self
    }

    pub(crate) fn union(&self, other: &Region) -> Region {
        Region {
            members: self
                .members
                .iter()
                .filter_map(|(axis, members)| {
                    let theirs = other.members.get(axis)?;
                    Some((*axis, members.union(theirs).copied().collect()))
                })
                .collect(),
        }
    }

    pub(crate) fn non_empty(self) -> Option<Region> {
        (!self.members.values().any(BTreeSet::is_empty)).then_some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_region_of_packed_coordinates_holds_only_the_members_that_changed() {
        let axes = [
            DimensionGlobalId(ulid::Ulid(1)),
            DimensionGlobalId(ulid::Ulid(2)),
        ];
        let packing = CoordinatePacking::new(&[8, 8]).expect("6 bits fit in a packed coordinate");
        let coordinates = [
            [MemberIndex(1), MemberIndex(5)],
            [MemberIndex(3), MemberIndex(5)],
        ];
        let packed_coordinates: Vec<u64> = coordinates
            .iter()
            .map(|coordinate| packing.pack(coordinate.iter().map(|member| member.0)))
            .collect();

        let region = Region::of_packed(&axes, &packing, &packed_coordinates);

        assert_eq!(
            region,
            Region::of_changes(&axes, coordinates.iter().map(|coordinate| &coordinate[..]))
        );
        assert_eq!(
            region.expect("cells changed").members(axes[0]),
            Some(&BTreeSet::from([1, 3]))
        );
    }
}
