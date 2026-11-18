use std::collections::BTreeMap;
use std::fmt;

use dimension::{Dimension, DimensionGlobalId, Member, MemberGlobalId, MemberIndex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coordinate(BTreeMap<DimensionGlobalId, MemberGlobalId>);

impl Coordinate {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (DimensionGlobalId, MemberGlobalId)> + '_ {
        self.0.iter().map(|(&axis, &member)| (axis, member))
    }

    pub fn named<'d>(
        declared: impl ExactSizeIterator<Item = &'d Dimension>,
        members: &[MemberIndex],
    ) -> Option<Coordinate> {
        if declared.len() != members.len() {
            return None;
        }
        let pairs = declared
            .zip(members)
            .map(|(dimension, &member)| {
                dimension
                    .member(member)
                    .map(|member| (dimension.global_id(), member.global_id()))
            })
            .collect::<Option<Vec<_>>>()?;
        Coordinate::try_new(pairs).ok()
    }
}

impl Coordinate {
    pub fn try_new(
        pairs: impl IntoIterator<Item = (DimensionGlobalId, MemberGlobalId)>,
    ) -> Result<Coordinate, DimensionGlobalId> {
        let mut members = BTreeMap::new();
        for (axis, member) in pairs {
            if members.insert(axis, member).is_some() {
                return Err(axis);
            }
        }
        Ok(Coordinate(members))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellError {
    Length {
        expected: usize,
        found: usize,
    },
    MissingAxis(DimensionGlobalId),
    RepeatedAxis(DimensionGlobalId),
    NoMember {
        axis: DimensionGlobalId,
        member: MemberGlobalId,
    },
}

pub fn resolve<'d>(
    declared: impl ExactSizeIterator<Item = &'d Dimension>,
    coordinate: &Coordinate,
) -> Result<Vec<MemberIndex>, CellError> {
    if coordinate.0.len() != declared.len() {
        return Err(CellError::Length {
            expected: declared.len(),
            found: coordinate.0.len(),
        });
    }
    declared
        .map(|dimension| {
            let axis = dimension.global_id();
            let member = *coordinate
                .0
                .get(&axis)
                .ok_or(CellError::MissingAxis(axis))?;
            dimension
                .member_by_global_id(member)
                .map(Member::id)
                .ok_or(CellError::NoMember { axis, member })
        })
        .collect()
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellError::Length { expected, found } => {
                write!(
                    f,
                    "the coordinate needs {expected} axes, but it has {found}"
                )
            }
            CellError::MissingAxis(axis) => {
                write!(f, "the coordinate has no member of axis {axis}")
            }
            CellError::RepeatedAxis(axis) => {
                write!(f, "the coordinate names axis {axis} more than once")
            }
            CellError::NoMember { axis, member } => {
                write!(f, "member {member} is not in dimension {axis}")
            }
        }
    }
}

impl std::error::Error for CellError {}
