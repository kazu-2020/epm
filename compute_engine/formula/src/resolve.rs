use std::ops::Range;

use dimension::DimensionDefinition;
use tile::TileDefinition;

use crate::expression::{Axis, Expression, ExpressionKind};
use crate::resolve_error::{ResolveError, ResolveErrorKind};
use crate::resolved_expression::{
    ResolvedAxis, ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub expression: ResolvedExpression,
    pub errors: Vec<ResolveError>,
}

#[must_use]
pub fn resolve(
    expression: &Expression,
    tiles: &[TileDefinition],
    dimensions: &[DimensionDefinition],
) -> Resolved {
    let mut resolver = Resolver {
        tiles,
        dimensions,
        errors: Vec::new(),
    };
    let expression = resolver.expression(expression);
    Resolved {
        expression,
        errors: resolver.errors,
    }
}

struct Resolver<'a> {
    tiles: &'a [TileDefinition],
    dimensions: &'a [DimensionDefinition],
    errors: Vec<ResolveError>,
}

impl<'a> Resolver<'a> {
    fn expression(&mut self, expression: &Expression) -> ResolvedExpression {
        let kind = match &expression.kind {
            ExpressionKind::Number(value) => ResolvedExpressionKind::Number(*value),
            ExpressionKind::Name(name) => self.tile(name).unwrap_or_else(|kind| {
                self.error(kind, &expression.span);
                ResolvedExpressionKind::Error
            }),
            ExpressionKind::Negate(operand) => {
                ResolvedExpressionKind::Negate(Box::new(self.expression(operand)))
            }
            ExpressionKind::Binary {
                operator,
                left,
                right,
            } => ResolvedExpressionKind::Binary {
                operator: *operator,
                left: Box::new(self.expression(left)),
                right: Box::new(self.expression(right)),
            },
            ExpressionKind::Sum { value, by } => ResolvedExpressionKind::Sum {
                value: Box::new(self.expression(value)),
                by: by.iter().map(|axis| self.axis(axis)).collect(),
            },
            ExpressionKind::Error => ResolvedExpressionKind::Error,
        };
        ResolvedExpression {
            kind,
            span: expression.span.clone(),
        }
    }

    fn tile(&self, name: &str) -> Result<ResolvedExpressionKind, ResolveErrorKind> {
        match (self.find_tile(name), self.find_dimension(name)) {
            (Some(tile), _) => Ok(ResolvedExpressionKind::Tile {
                ulid: tile.ulid,
                axes: tile.axes.clone(),
            }),
            (None, Some(_)) => Err(ResolveErrorKind::DimensionAsValue),
            (None, None) => Err(ResolveErrorKind::UnknownName),
        }
    }

    fn axis(&mut self, axis: &Axis) -> ResolvedAxis {
        let kind = self.axis_kind(axis).unwrap_or_else(|kind| {
            self.error(kind, &axis.span);
            ResolvedAxisKind::Unresolved
        });
        ResolvedAxis {
            kind,
            span: axis.span.clone(),
        }
    }

    fn axis_kind(&self, axis: &Axis) -> Result<ResolvedAxisKind, ResolveErrorKind> {
        let dimension = match (
            self.find_dimension(&axis.dimension),
            self.find_tile(&axis.dimension),
        ) {
            (Some(dimension), _) => dimension,
            (None, Some(_)) => return Err(ResolveErrorKind::TileAsAxis),
            (None, None) => return Err(ResolveErrorKind::UnknownName),
        };
        Ok(match &axis.attribute {
            None => ResolvedAxisKind::Dimension(dimension.ulid),
            Some(name) => {
                let Some(attribute) = dimension
                    .attributes
                    .iter()
                    .find(|attribute| attribute.name == *name)
                else {
                    return Err(ResolveErrorKind::UnknownAttribute);
                };
                ResolvedAxisKind::Attribute {
                    dimension: dimension.ulid,
                    attribute: attribute.ulid,
                    target: attribute.target,
                }
            }
        })
    }

    fn find_tile(&self, name: &str) -> Option<&'a TileDefinition> {
        self.tiles.iter().find(|tile| tile.name == name)
    }

    fn find_dimension(&self, name: &str) -> Option<&'a DimensionDefinition> {
        self.dimensions
            .iter()
            .find(|dimension| dimension.name == name)
    }

    fn error(&mut self, kind: ResolveErrorKind, span: &Range<usize>) {
        self.errors.push(ResolveError {
            kind,
            span: span.clone(),
        });
    }
}
