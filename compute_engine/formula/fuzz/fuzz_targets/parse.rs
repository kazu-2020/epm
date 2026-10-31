#![no_main]

use std::ops::Range;

use formula::{Expression, ExpressionKind, parse};
use libfuzzer_sys::fuzz_target;

fn assert_inside(source: &str, span: &Range<usize>, outer: &Range<usize>) {
    assert!(span.start <= span.end, "{span:?} is reversed");
    assert!(
        outer.start <= span.start && span.end <= outer.end,
        "{span:?} is outside {outer:?}"
    );
    assert!(
        source.is_char_boundary(span.start) && source.is_char_boundary(span.end),
        "{span:?} splits a character"
    );
}

fn check(source: &str, expression: &Expression, outer: &Range<usize>) -> usize {
    assert_inside(source, &expression.span, outer);
    match &expression.kind {
        ExpressionKind::Negate(operand) => 1 + check(source, operand, &expression.span),
        ExpressionKind::Binary { left, right, .. } => {
            assert!(left.span.end <= right.span.start, "operands overlap");
            let left = check(source, left, &expression.span);
            let right = check(source, right, &expression.span);
            1 + left.max(right)
        }
        ExpressionKind::Sum { value, by } => {
            let height = check(source, value, &expression.span);
            let mut previous_end = value.span.end;
            for axis in by {
                assert_inside(source, &axis.span, &expression.span);
                assert!(previous_end <= axis.span.start, "axes overlap");
                previous_end = axis.span.end;
            }
            1 + height
        }
        ExpressionKind::Number(_) | ExpressionKind::Name(_) | ExpressionKind::Error => 0,
    }
}

fuzz_target!(|source: &str| {
    let parsed = parse(source);
    let whole = 0..source.len();
    let height = check(source, &parsed.expression, &whole);
    assert!(height <= 128, "the tree is {height} levels high");
    for error in &parsed.errors {
        assert_inside(source, &error.span, &whole);
    }
});
