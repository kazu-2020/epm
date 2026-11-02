use std::collections::BTreeSet;
use std::ops::Range;

use dimension::DimensionUlid;

use crate::axis_error::{AxisError, AxisErrorKind};
use crate::resolved_expression::{
    ResolvedAxis, ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisCheck {
    pub broadcast: Option<Vec<DimensionUlid>>,
    pub errors: Vec<AxisError>,
}

#[must_use]
pub fn check_axes(expression: &ResolvedExpression, result_axes: &[DimensionUlid]) -> AxisCheck {
    let mut checker = Checker { errors: Vec::new() };
    let Some(axes) = checker.axes(expression, Some(result_axes)) else {
        return AxisCheck {
            broadcast: None,
            errors: checker.errors,
        };
    };
    for &axis in axes.iter().filter(|axis| !result_axes.contains(axis)) {
        checker.error(
            AxisErrorKind::ReducesAxesWithoutAggregation,
            axis,
            &expression.span,
        );
    }
    AxisCheck {
        broadcast: Some(
            result_axes
                .iter()
                .copied()
                .filter(|axis| !axes.contains(axis))
                .collect(),
        ),
        errors: checker.errors,
    }
}

struct Checker {
    errors: Vec<AxisError>,
}

impl Checker {
    fn axes(
        &mut self,
        expression: &ResolvedExpression,
        allowed: Option<&[DimensionUlid]>,
    ) -> Option<BTreeSet<DimensionUlid>> {
        match &expression.kind {
            ResolvedExpressionKind::Number(_) => Some(BTreeSet::new()),
            ResolvedExpressionKind::Measure { axes, .. } => Some(axes.iter().copied().collect()),
            ResolvedExpressionKind::Negate(operand) => self.axes(operand, allowed),
            ResolvedExpressionKind::Binary { left, right, .. } => {
                // Not `self.axes(left, allowed)?`: an unknown left side would skip the right side's checks.
                let left = self.axes(left, allowed);
                let right = self.axes(right, allowed);
                let mut axes = left?;
                axes.extend(right?);
                Some(axes)
            }
            ResolvedExpressionKind::Sum { value, by } => {
                // Not `self.axes(value, None)?`: BY is still checked when the value's axes are unknown.
                let value = self.axes(value, None);
                let axes = self.sum(value.as_ref(), by, allowed);
                value.and(axes)
            }
            ResolvedExpressionKind::Error => None,
        }
    }

    fn sum(
        &mut self,
        value: Option<&BTreeSet<DimensionUlid>>,
        by: &[ResolvedAxis],
        allowed: Option<&[DimensionUlid]>,
    ) -> Option<BTreeSet<DimensionUlid>> {
        let mut axes = BTreeSet::new();
        let mut known = true;
        for axis in by {
            let (source, output) = match axis.kind {
                ResolvedAxisKind::Dimension(dimension) => (dimension, dimension),
                ResolvedAxisKind::Attribute {
                    dimension, target, ..
                } => (dimension, target),
                ResolvedAxisKind::Unresolved => {
                    known = false;
                    continue;
                }
            };
            let error = if value.is_some_and(|value| !value.contains(&source)) {
                Some((AxisErrorKind::ValueLacksAxis, source))
            } else if allowed.is_some_and(|allowed| !allowed.contains(&output)) {
                Some((AxisErrorKind::AxisNotInResult, output))
            } else if !axes.insert(output) {
                Some((AxisErrorKind::DuplicateAxis, output))
            } else {
                None
            };
            if let Some((kind, dimension)) = error {
                self.error(kind, dimension, &axis.span);
                known = false;
            }
        }
        known.then_some(axes)
    }

    fn error(&mut self, kind: AxisErrorKind, dimension: DimensionUlid, span: &Range<usize>) {
        self.errors.push(AxisError {
            kind,
            dimension,
            span: span.clone(),
        });
    }
}
