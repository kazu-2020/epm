use std::ops::Range;

use dimension::{Dimension, MemberIndex};
use measure::MeasureDefinition;

use crate::expression::{Axis, Expression, ExpressionKind, QuotedName};
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
    measures: &[MeasureDefinition],
    dimensions: &[&Dimension],
) -> Resolved {
    let mut resolver = Resolver {
        measures,
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
    measures: &'a [MeasureDefinition],
    dimensions: &'a [&'a Dimension],
    errors: Vec<ResolveError>,
}

impl<'a> Resolver<'a> {
    fn expression(&mut self, expression: &Expression) -> ResolvedExpression {
        let kind = match &expression.kind {
            ExpressionKind::Number(value) => ResolvedExpressionKind::Number(*value),
            ExpressionKind::Name(name) => self.measure(name).unwrap_or_else(|kind| {
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
            ExpressionKind::Filter {
                value,
                dimension,
                members,
                keeps_axis,
            } => {
                let value = self.expression(value);
                self.filter(value, dimension, members, *keeps_axis)
                    .unwrap_or(ResolvedExpressionKind::Error)
            }
            ExpressionKind::Error => ResolvedExpressionKind::Error,
        };
        ResolvedExpression {
            kind,
            span: expression.span.clone(),
        }
    }

    fn filter(
        &mut self,
        value: ResolvedExpression,
        dimension: &QuotedName,
        members: &[QuotedName],
        keeps_axis: bool,
    ) -> Option<ResolvedExpressionKind> {
        let found = match (
            self.find_dimension(&dimension.name),
            self.find_measure(&dimension.name),
        ) {
            (Some(found), _) => found,
            (None, Some(_)) => {
                self.error(ResolveErrorKind::MeasureAsAxis, &dimension.span);
                return None;
            }
            (None, None) => {
                self.error(ResolveErrorKind::UnknownName, &dimension.span);
                return None;
            }
        };
        let mut ids: Vec<MemberIndex> = Vec::with_capacity(members.len());
        let mut known = true;
        for member in members {
            match found.member_by_key(&member.name) {
                Some(found_member) => ids.push(found_member.id()),
                None => {
                    self.error(ResolveErrorKind::UnknownMember, &member.span);
                    known = false;
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        known.then(|| ResolvedExpressionKind::Filter {
            value: Box::new(value),
            dimension: found.ulid(),
            members: ids,
            keeps_axis,
        })
    }

    fn measure(&self, name: &str) -> Result<ResolvedExpressionKind, ResolveErrorKind> {
        match (self.find_measure(name), self.find_dimension(name)) {
            (Some(measure), _) => Ok(ResolvedExpressionKind::Measure {
                ulid: measure.ulid,
                axes: measure.axes.clone(),
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
            self.find_measure(&axis.dimension),
        ) {
            (Some(dimension), _) => dimension,
            (None, Some(_)) => return Err(ResolveErrorKind::MeasureAsAxis),
            (None, None) => return Err(ResolveErrorKind::UnknownName),
        };
        let dimension = dimension.definition();
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

    fn find_measure(&self, name: &str) -> Option<&'a MeasureDefinition> {
        self.measures.iter().find(|measure| measure.name == name)
    }

    fn find_dimension(&self, name: &str) -> Option<&'a Dimension> {
        self.dimensions
            .iter()
            .find(|dimension| dimension.name() == name)
            .copied()
    }

    fn error(&mut self, kind: ResolveErrorKind, span: &Range<usize>) {
        self.errors.push(ResolveError {
            kind,
            span: span.clone(),
        });
    }
}
