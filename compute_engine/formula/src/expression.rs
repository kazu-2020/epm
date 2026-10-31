use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    pub kind: ExpressionKind,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExpressionKind {
    Number(f64),
    Name(String),
    Negate(Box<Expression>),
    Binary {
        operator: BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Sum {
        value: Box<Expression>,
        by: Vec<Axis>,
    },
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Axis {
    pub dimension: String,
    pub attribute: Option<String>,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
}
