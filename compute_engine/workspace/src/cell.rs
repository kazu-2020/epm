use std::fmt;

use dimension::{Dimension, DimensionGlobalId, Member, MemberGlobalId, MemberIndex};

pub type Coordinate = Vec<(DimensionGlobalId, MemberGlobalId)>;

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

pub fn resolve(
    declared: &[&Dimension],
    coordinate: &[(DimensionGlobalId, MemberGlobalId)],
) -> Result<Vec<MemberIndex>, CellError> {
    if coordinate.len() != declared.len() {
        return Err(CellError::Length {
            expected: declared.len(),
            found: coordinate.len(),
        });
    }
    declared
        .iter()
        .map(|dimension| {
            let axis = dimension.global_id();
            let &(_, member) = coordinate
                .iter()
                .find(|&&(named, _)| named == axis)
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
