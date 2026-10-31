use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;

use crate::syntax_error::{SyntaxError, SyntaxErrorKind};

#[derive(Debug)]
pub(crate) enum TokenKind {
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

#[derive(Debug)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) span: Range<usize>,
    pub(crate) reported: bool,
}

pub(crate) fn tokenize(source: &str) -> (Vec<Token>, Vec<SyntaxError>) {
    let mut lexer = Lexer {
        source,
        characters: source.char_indices().peekable(),
        errors: Vec::new(),
    };
    let tokens = lexer.tokens();
    (tokens, lexer.errors)
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
