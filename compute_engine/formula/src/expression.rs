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
    Filter {
        value: Box<Expression>,
        dimension: QuotedName,
        members: Vec<QuotedName>,
    },
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotedName {
    pub name: String,
    pub span: Range<usize>,
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

impl BinaryOperator {
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOperator::Add => "+",
            BinaryOperator::Subtract => "-",
            BinaryOperator::Multiply => "*",
            BinaryOperator::Divide => "/",
        }
    }
}
