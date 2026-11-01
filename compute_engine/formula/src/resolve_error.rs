use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    pub kind: ResolveErrorKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveErrorKind {
    UnknownName,
    DimensionAsValue,
    UnknownAttribute,
    TileAsAxis,
}
