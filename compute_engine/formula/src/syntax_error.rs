use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxErrorKind {
    UnterminatedName,
    UnexpectedCharacter,
    ExpectedExpression,
    ExpectedClosingParenthesis,
    UnexpectedToken,
    TooDeeplyNested,
    NumberOutOfRange,
    UnquotedName,
    EmptyName,
    ExpectedOpeningParenthesis,
    MoreThanOneValue,
    EmptyBy,
    ByNotLast,
    ExpressionInBy,
    ExpectedAxis,
}
