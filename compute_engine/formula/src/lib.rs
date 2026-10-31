use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;
use std::vec;

const MAXIMUM_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub expression: Expression,
    pub errors: Vec<SyntaxError>,
}

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

#[derive(Debug)]
enum TokenKind {
    Number(f64),
    Name(String),
    Word,
    Sum,
    By,
    Plus,
    Minus,
    Asterisk,
    Slash,
    LeftParenthesis,
    RightParenthesis,
    Comma,
    Point,
}

impl TokenKind {
    fn additive_operator(&self) -> Option<BinaryOperator> {
        match self {
            TokenKind::Plus => Some(BinaryOperator::Add),
            TokenKind::Minus => Some(BinaryOperator::Subtract),
            _ => None,
        }
    }

    fn multiplicative_operator(&self) -> Option<BinaryOperator> {
        match self {
            TokenKind::Asterisk => Some(BinaryOperator::Multiply),
            TokenKind::Slash => Some(BinaryOperator::Divide),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Token {
    kind: TokenKind,
    span: Range<usize>,
    reported: bool,
}

pub fn parse(source: &str) -> Parsed {
    let mut lexer = Lexer {
        source,
        characters: source.char_indices().peekable(),
        errors: Vec::new(),
    };
    let tokens = lexer.tokens();
    let mut parser = Parser {
        tokens: tokens.into_iter().peekable(),
        end: source.len(),
        previous_end: 0,
        errors: lexer.errors,
    };
    let expression = match parser.expression(0) {
        Ok(subtree) => {
            parser.reject_trailing_tokens();
            subtree.expression
        }
        Err(TooDeep { position }) => {
            parser.error(SyntaxErrorKind::TooDeeplyNested, position..source.len());
            Expression {
                kind: ExpressionKind::Error,
                span: 0..source.len(),
            }
        }
    };
    parser.errors.sort_by_key(|error| error.span.start);
    Parsed {
        expression,
        errors: parser.errors,
    }
}

struct Lexer<'source> {
    source: &'source str,
    characters: Peekable<CharIndices<'source>>,
    errors: Vec<SyntaxError>,
}

impl Lexer<'_> {
    fn tokens(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some((start, character)) = self.characters.next() {
            let errors = self.errors.len();
            let kind = match character {
                '\'' => self.name(start),
                '0'..='9' => self.number(start),
                character if character.is_whitespace() => continue,
                character if starts_word(character) => {
                    self.skip_while(|next| !ends_word(next));
                    match &self.source[start..self.offset()] {
                        "SUM" => TokenKind::Sum,
                        "BY" => TokenKind::By,
                        _ => TokenKind::Word,
                    }
                }
                '.' if self.attribute_follows() => TokenKind::Point,
                character => match punctuation(character) {
                    Some(kind) => kind,
                    None => {
                        self.skip_while(|next| !starts_token(next));
                        let end = self.offset();
                        self.error(SyntaxErrorKind::UnexpectedCharacter, start..end);
                        continue;
                    }
                },
            };
            tokens.push(Token {
                kind,
                span: start..self.offset(),
                reported: self.errors.len() > errors,
            });
        }
        tokens
    }

    fn name(&mut self, start: usize) -> TokenKind {
        let mut name = String::new();
        loop {
            match self.characters.next() {
                Some((end, '\'')) => {
                    if self.characters.next_if(|&(_, next)| next == '\'').is_none() {
                        if name.is_empty() {
                            self.error(SyntaxErrorKind::EmptyName, start..end + 1);
                        }
                        return TokenKind::Name(name);
                    }
                    name.push('\'');
                }
                Some((_, character)) => name.push(character),
                None => {
                    self.error(SyntaxErrorKind::UnterminatedName, start..self.source.len());
                    return TokenKind::Name(name);
                }
            }
        }
    }

    fn number(&mut self, start: usize) -> TokenKind {
        self.skip_while(|next| next.is_ascii_digit());
        let mut lookahead = self.characters.clone();
        if lookahead.next().is_some_and(|(_, next)| next == '.')
            && lookahead
                .next()
                .is_some_and(|(_, next)| next.is_ascii_digit())
        {
            self.characters.next();
            self.skip_while(|next| next.is_ascii_digit());
        }
        let end = self.offset();
        let value: f64 = self.source[start..end]
            .parse()
            .expect("digits with an optional fraction parse as f64");
        if !value.is_finite() {
            self.error(SyntaxErrorKind::NumberOutOfRange, start..end);
        }
        TokenKind::Number(value)
    }

    fn attribute_follows(&self) -> bool {
        // Not a token on its own: "1." and ".5" stay unexpected characters next to the number instead of splitting it.
        let mut lookahead = self.characters.clone();
        lookahead
            .find(|&(_, next)| !next.is_whitespace())
            .is_some_and(|(_, next)| next == '\'')
    }

    fn skip_while(&mut self, continues: impl Fn(char) -> bool) {
        while self
            .characters
            .next_if(|&(_, next)| continues(next))
            .is_some()
        {}
    }

    fn offset(&mut self) -> usize {
        self.characters
            .peek()
            .map_or(self.source.len(), |&(index, _)| index)
    }

    fn error(&mut self, kind: SyntaxErrorKind, span: Range<usize>) {
        self.errors.push(SyntaxError { kind, span });
    }
}

fn starts_word(character: char) -> bool {
    character.is_alphabetic() || character == '_'
}

fn punctuation(character: char) -> Option<TokenKind> {
    match character {
        '+' => Some(TokenKind::Plus),
        '-' => Some(TokenKind::Minus),
        '*' => Some(TokenKind::Asterisk),
        '/' => Some(TokenKind::Slash),
        '(' => Some(TokenKind::LeftParenthesis),
        ')' => Some(TokenKind::RightParenthesis),
        ',' => Some(TokenKind::Comma),
        _ => None,
    }
}

fn ends_word(character: char) -> bool {
    character.is_whitespace() || character == '\'' || punctuation(character).is_some()
}

fn starts_token(character: char) -> bool {
    ends_word(character) || character.is_ascii_digit() || starts_word(character)
}

struct Parser {
    tokens: Peekable<vec::IntoIter<Token>>,
    end: usize,
    previous_end: usize,
    errors: Vec<SyntaxError>,
}

struct Subtree {
    expression: Expression,
    height: usize,
}

struct TooDeep {
    position: usize,
}

type Parsing = Result<Subtree, TooDeep>;

impl Parser {
    fn expression(&mut self, depth: usize) -> Parsing {
        let mut left = self.term(depth)?;
        while let Some(operator) = self.peek_operator(TokenKind::additive_operator) {
            left = self.binary(depth, operator, left, Self::term)?;
        }
        Ok(left)
    }

    fn term(&mut self, depth: usize) -> Parsing {
        let mut left = self.factor(depth)?;
        while let Some(operator) = self.peek_operator(TokenKind::multiplicative_operator) {
            left = self.binary(depth, operator, left, Self::factor)?;
        }
        Ok(left)
    }

    fn binary(
        &mut self,
        depth: usize,
        operator: BinaryOperator,
        left: Subtree,
        operand: fn(&mut Self, usize) -> Parsing,
    ) -> Parsing {
        self.descend(depth + 1 + left.height)?;
        self.next_token();
        let right = operand(self, depth + 1)?;
        Ok(Subtree {
            height: 1 + left.height.max(right.height),
            expression: Expression {
                span: left.expression.span.start..right.expression.span.end,
                kind: ExpressionKind::Binary {
                    operator,
                    left: Box::new(left.expression),
                    right: Box::new(right.expression),
                },
            },
        })
    }

    fn factor(&mut self, depth: usize) -> Parsing {
        match self.tokens.peek().map(|token| &token.kind) {
            Some(TokenKind::Minus) => {
                self.descend(depth + 1)?;
                let minus = self.next_token();
                let operand = self.factor(depth + 1)?;
                Ok(Subtree {
                    height: 1 + operand.height,
                    expression: Expression {
                        span: minus.span.start..operand.expression.span.end,
                        kind: ExpressionKind::Negate(Box::new(operand.expression)),
                    },
                })
            }
            Some(TokenKind::LeftParenthesis) => {
                self.descend(depth + 1)?;
                let opening = self.next_token();
                let inside = self.expression(depth + 1)?;
                let end = self.closing_parenthesis(inside.expression.span.end);
                Ok(Subtree {
                    height: 1 + inside.height,
                    expression: Expression {
                        span: opening.span.start..end,
                        ..inside.expression
                    },
                })
            }
            Some(TokenKind::Sum) => self.sum(depth),
            Some(TokenKind::Number(_) | TokenKind::Name(_) | TokenKind::Word) => {
                let token = self.next_token();
                let kind = match token.kind {
                    TokenKind::Number(value) if !value.is_finite() => ExpressionKind::Error,
                    TokenKind::Number(value) => ExpressionKind::Number(value),
                    TokenKind::Name(name) if name.is_empty() => ExpressionKind::Error,
                    TokenKind::Name(name) => ExpressionKind::Name(name),
                    TokenKind::Word => {
                        self.error(SyntaxErrorKind::UnquotedName, token.span.clone());
                        ExpressionKind::Error
                    }
                    _ => unreachable!("peeked a number, a name or a word"),
                };
                Ok(Subtree::leaf(kind, token.span))
            }
            _ => {
                let position = self.position();
                self.error(SyntaxErrorKind::ExpectedExpression, position..position);
                Ok(Subtree::leaf(ExpressionKind::Error, position..position))
            }
        }
    }

    fn sum(&mut self, depth: usize) -> Parsing {
        self.descend(depth + 1)?;
        let keyword = self.next_token();
        if self
            .next_if(|kind| matches!(kind, TokenKind::LeftParenthesis))
            .is_none()
        {
            let position = self.position();
            self.error(
                SyntaxErrorKind::ExpectedOpeningParenthesis,
                position..position,
            );
            return Ok(Subtree::leaf(ExpressionKind::Error, keyword.span));
        }
        let mut by = Vec::new();
        let value = match self.next_if(|kind| matches!(kind, TokenKind::By)) {
            Some(keyword) => {
                self.error(SyntaxErrorKind::ByNotLast, keyword.span.clone());
                let start = keyword.span.start;
                by = self.by(keyword.span);
                Subtree::leaf(ExpressionKind::Error, start..start)
            }
            None => self.expression(depth + 1)?,
        };
        let mut extra_values: Option<Range<usize>> = None;
        while self
            .next_if(|kind| matches!(kind, TokenKind::Comma))
            .is_some()
        {
            if let Some(keyword) = self.next_if(|kind| matches!(kind, TokenKind::By)) {
                by = self.by(keyword.span);
                break;
            }
            let extra = self.expression(depth + 1)?;
            if extra.expression.span.is_empty() {
                continue;
            }
            let span = extra.expression.span;
            let start = extra_values.map_or(span.start, |run| run.start);
            extra_values = Some(start..span.end);
        }
        if let Some(span) = extra_values {
            self.error(SyntaxErrorKind::MoreThanOneValue, span);
        }
        let end = self.closing_parenthesis(self.previous_end.max(value.expression.span.end));
        Ok(Subtree {
            height: 1 + value.height,
            expression: Expression {
                kind: ExpressionKind::Sum {
                    value: Box::new(value.expression),
                    by,
                },
                span: keyword.span.start..end,
            },
        })
    }

    fn by(&mut self, keyword: Range<usize>) -> Vec<Axis> {
        let mut axes = Vec::new();
        if self.at_argument_end() {
            self.error(SyntaxErrorKind::EmptyBy, keyword);
            return axes;
        }
        loop {
            if let Some(again) = self.next_if(|kind| matches!(kind, TokenKind::By)) {
                self.error(SyntaxErrorKind::ByNotLast, again.span);
            }
            axes.extend(self.axis());
            if self
                .next_if(|kind| matches!(kind, TokenKind::Comma))
                .is_none()
            {
                return axes;
            }
        }
    }

    fn axis(&mut self) -> Option<Axis> {
        let start = self.position();
        if self.at_argument_end() {
            self.error(SyntaxErrorKind::ExpectedAxis, start..start);
            return None;
        }
        if let Some((dimension, span)) = self.name() {
            let mut axis = Axis {
                dimension,
                attribute: None,
                span,
            };
            if self
                .next_if(|kind| matches!(kind, TokenKind::Point))
                .is_some()
            {
                let (attribute, span) = self
                    .name()
                    .expect("the lexer makes a point only before a name");
                axis.attribute = Some(attribute);
                axis.span.end = span.end;
            }
            if self.at_argument_end() {
                let empty = axis.dimension.is_empty() || axis.attribute.as_deref() == Some("");
                return (!empty).then_some(axis);
            }
        }
        self.skip_expression_in_by(start);
        None
    }

    fn name(&mut self) -> Option<(String, Range<usize>)> {
        let token = self.next_if(|kind| matches!(kind, TokenKind::Name(_)))?;
        let TokenKind::Name(name) = token.kind else {
            unreachable!("took a name");
        };
        Some((name, token.span))
    }

    fn skip_expression_in_by(&mut self, start: usize) {
        let unquoted_axis = self.position() == start
            && matches!(
                self.tokens.peek().map(|token| &token.kind),
                Some(TokenKind::Word)
            );
        let mut end = start;
        let mut tokens: usize = 0;
        let mut open_parentheses = 0usize;
        while let Some(token) = self.next_if(|kind| {
            open_parentheses > 0 || !matches!(kind, TokenKind::Comma | TokenKind::RightParenthesis)
        }) {
            match token.kind {
                TokenKind::LeftParenthesis => open_parentheses += 1,
                TokenKind::RightParenthesis => open_parentheses -= 1,
                TokenKind::Word => self.error(SyntaxErrorKind::UnquotedName, token.span.clone()),
                _ => {}
            }
            tokens += 1;
            end = token.span.end;
        }
        if !(unquoted_axis && tokens == 1) {
            self.error(SyntaxErrorKind::ExpressionInBy, start..end);
        }
    }

    fn at_argument_end(&mut self) -> bool {
        self.tokens.peek().is_none_or(|token| {
            matches!(token.kind, TokenKind::Comma | TokenKind::RightParenthesis)
        })
    }

    fn closing_parenthesis(&mut self, end_without_it: usize) -> usize {
        match self.next_if(|kind| matches!(kind, TokenKind::RightParenthesis)) {
            Some(closing) => closing.span.end,
            None => {
                let position = self.position();
                self.error(
                    SyntaxErrorKind::ExpectedClosingParenthesis,
                    position..position,
                );
                end_without_it
            }
        }
    }

    fn next_if(&mut self, matches: impl Fn(&TokenKind) -> bool) -> Option<Token> {
        let token = self.tokens.next_if(|token| matches(&token.kind))?;
        self.previous_end = token.span.end;
        Some(token)
    }

    fn descend(&mut self, depth: usize) -> Result<(), TooDeep> {
        // Not unbounded: parsing recurses once per parenthesis, negation or operand level and later passes once per tree level, and a formula this deep belongs in separate named tiles.
        if depth > MAXIMUM_DEPTH {
            return Err(TooDeep {
                position: self.position(),
            });
        }
        Ok(())
    }

    fn peek_operator(
        &mut self,
        operator: fn(&TokenKind) -> Option<BinaryOperator>,
    ) -> Option<BinaryOperator> {
        self.tokens.peek().and_then(|token| operator(&token.kind))
    }

    fn next_token(&mut self) -> Token {
        let token = self.tokens.next().expect("peeked a token");
        self.previous_end = token.span.end;
        token
    }

    fn reject_trailing_tokens(&mut self) {
        let mut unexpected: Option<Range<usize>> = None;
        while let Some(token) = self.tokens.next() {
            let word = matches!(token.kind, TokenKind::Word);
            if !word && !token.reported {
                let start = unexpected.map_or(token.span.start, |run| run.start);
                unexpected = Some(start..token.span.end);
                continue;
            }
            self.reject_unexpected(unexpected.take());
            if word {
                self.error(SyntaxErrorKind::UnquotedName, token.span);
            }
        }
        self.reject_unexpected(unexpected);
    }

    fn reject_unexpected(&mut self, run: Option<Range<usize>>) {
        if let Some(span) = run {
            self.error(SyntaxErrorKind::UnexpectedToken, span);
        }
    }

    fn position(&mut self) -> usize {
        self.tokens
            .peek()
            .map_or(self.end, |token| token.span.start)
    }

    fn error(&mut self, kind: SyntaxErrorKind, span: Range<usize>) {
        self.errors.push(SyntaxError { kind, span });
    }
}

impl Subtree {
    fn leaf(kind: ExpressionKind, span: Range<usize>) -> Self {
        Subtree {
            expression: Expression { kind, span },
            height: 0,
        }
    }
}
