use std::collections::{BTreeMap, BTreeSet};

use dimension::{DimensionUlid, MemberId};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Region {
    // A dimension left out spans all of its members.
    members: BTreeMap<DimensionUlid, BTreeSet<u32>>,
}

impl Region {
    pub fn of_changes<'c>(
        axes: &[DimensionUlid],
        coordinates: impl IntoIterator<Item = &'c [MemberId]>,
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

    pub fn members(&self, dimension: DimensionUlid) -> Option<&BTreeSet<u32>> {
        self.members.get(&dimension)
    }

    pub(crate) fn constrain(&mut self, dimension: DimensionUlid, members: BTreeSet<u32>) {
        let members = match self.members.get(&dimension) {
            Some(kept) => kept.intersection(&members).copied().collect(),
            None => members,
        };
        self.members.insert(dimension, members);
    }

    pub(crate) fn without(mut self, dimension: DimensionUlid) -> Region {
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
