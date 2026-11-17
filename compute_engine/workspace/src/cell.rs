use std::fmt;

use dimension::{Dimension, DimensionGlobalId, Member, MemberGlobalId, MemberIndex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coordinate(Vec<(DimensionGlobalId, MemberGlobalId)>);

impl Coordinate {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (DimensionGlobalId, MemberGlobalId)> + '_ {
        self.0.iter().copied()
    }

    pub fn named<'d>(
        declared: impl ExactSizeIterator<Item = &'d Dimension>,
        members: &[MemberIndex],
    ) -> Option<Coordinate> {
        if declared.len() != members.len() {
            return None;
        }
        declared
            .zip(members)
            .map(|(dimension, &member)| {
                dimension
                    .member(member)
                    .map(|member| (dimension.global_id(), member.global_id()))
            })
            .collect()
    }
}

impl FromIterator<(DimensionGlobalId, MemberGlobalId)> for Coordinate {
    fn from_iter<I: IntoIterator<Item = (DimensionGlobalId, MemberGlobalId)>>(pairs: I) -> Self {
        Coordinate(pairs.into_iter().collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellError {
    Length {
        expected: usize,
        found: usize,
    },
    MissingAxis(DimensionGlobalId),
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
    // Not a check for a repeated axis: with equal lengths, a repeated axis leaves a declared axis missing.
    declared
        .map(|dimension| {
            let axis = dimension.global_id();
            let (_, member) = coordinate
                .iter()
                .find(|&(named, _)| named == axis)
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
            CellError::NoMember { axis, member } => {
                write!(f, "member {member} is not in dimension {axis}")
            }
        }
    }
}

impl std::error::Error for CellError {}
