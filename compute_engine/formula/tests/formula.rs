use std::ops::Range;

use formula::{Axis, Expression, ExpressionKind, SyntaxError, SyntaxErrorKind};

fn render(expression: &Expression) -> String {
    match &expression.kind {
        ExpressionKind::Number(value) => value.to_string(),
        ExpressionKind::Name(name) => format!("'{name}'"),
        ExpressionKind::Negate(operand) => format!("(Negate {})", render(operand)),
        ExpressionKind::Binary {
            operator,
            left,
            right,
        } => format!("({operator:?} {} {})", render(left), render(right)),
        ExpressionKind::Sum { value, by } if by.is_empty() => format!("(Sum {})", render(value)),
        ExpressionKind::Sum { value, by } => {
            let axes: Vec<_> = by.iter().map(render_axis).collect();
            format!("(Sum {} BY {})", render(value), axes.join(" "))
        }
        ExpressionKind::Error => "ERROR".into(),
    }
}

fn render_axis(axis: &Axis) -> String {
    match &axis.attribute {
        Some(attribute) => format!("'{}'.'{attribute}'", axis.dimension),
        None => format!("'{}'", axis.dimension),
    }
}

fn tree(source: &str) -> String {
    let parsed = formula::parse(source);
    assert_eq!(parsed.errors, [], "{source}");
    render(&parsed.expression)
}

#[track_caller]
fn assert_outcome(source: &str, tree: &str, errors: &[(SyntaxErrorKind, Range<usize>)]) {
    let parsed = formula::parse(source);
    assert_eq!(render(&parsed.expression), tree, "{source}");
    let actual: Vec<_> = parsed
        .errors
        .into_iter()
        .map(|SyntaxError { kind, span }| (kind, span))
        .collect();
    assert_eq!(actual, errors, "{source}");
}

fn errors(source: &str) -> Vec<(SyntaxErrorKind, Range<usize>)> {
    formula::parse(source)
        .errors
        .into_iter()
        .map(|SyntaxError { kind, span }| (kind, span))
        .collect()
}

#[test]
fn multiplication_and_division_bind_tighter_than_addition_and_subtraction() {
    assert_eq!(tree("1 + 2 * 3"), "(Add 1 (Multiply 2 3))");
    assert_eq!(tree("1 * 2 + 3"), "(Add (Multiply 1 2) 3)");
    assert_eq!(tree("1 - 2 / 3"), "(Subtract 1 (Divide 2 3))");
    assert_eq!(tree("1 / 2 - 3"), "(Subtract (Divide 1 2) 3)");
}

#[test]
fn binary_operators_of_equal_precedence_group_from_the_left() {
    assert_eq!(tree("1 - 2 - 3"), "(Subtract (Subtract 1 2) 3)");
    assert_eq!(tree("8 / 4 / 2"), "(Divide (Divide 8 4) 2)");
    assert_eq!(tree("1 - 2 + 3"), "(Add (Subtract 1 2) 3)");
    assert_eq!(tree("1 / 2 * 3"), "(Multiply (Divide 1 2) 3)");
}

#[test]
fn unary_minus_binds_tighter_than_every_binary_operator() {
    assert_eq!(tree("-'a' * 'b'"), "(Multiply (Negate 'a') 'b')");
    assert_eq!(tree("-2 - 3"), "(Subtract (Negate 2) 3)");
    assert_eq!(tree("2 * -3"), "(Multiply 2 (Negate 3))");
    assert_eq!(tree("1 - -2"), "(Subtract 1 (Negate 2))");
    assert_eq!(tree("- - 1"), "(Negate (Negate 1))");
}

#[test]
fn parentheses_override_precedence_and_nest() {
    assert_eq!(tree("(1 + 2) * 3"), "(Multiply (Add 1 2) 3)");
    assert_eq!(
        tree("((1 + 2)) * (3 - (4 / 5))"),
        "(Multiply (Add 1 2) (Subtract 3 (Divide 4 5)))"
    );
    assert_eq!(tree("-(1 + 2)"), "(Negate (Add 1 2))");
}

#[test]
fn names_are_quoted_and_unescape_doubled_quotes() {
    assert_eq!(tree("'単価' * '数量'"), "(Multiply '単価' '数量')");
    assert_eq!(tree("'McDonald''s'"), "'McDonald's'");
    assert_eq!(tree("''''"), "'''");
}

#[test]
fn an_empty_name_is_an_error() {
    assert_outcome("''", "ERROR", &[(SyntaxErrorKind::EmptyName, 0..2)]);
}

#[test]
fn an_unquoted_word_is_one_error_asking_for_quotes() {
    assert_outcome(
        "price * 単価2",
        "(Multiply ERROR ERROR)",
        &[
            (SyntaxErrorKind::UnquotedName, 0..5),
            (SyntaxErrorKind::UnquotedName, 8..15),
        ],
    );
    assert_eq!(
        errors("か\u{3099} * 1"),
        [(SyntaxErrorKind::UnquotedName, 0..6)]
    );
    assert_outcome(
        "price*'単価'",
        "(Multiply ERROR '単価')",
        &[(SyntaxErrorKind::UnquotedName, 0..5)],
    );
    assert_outcome(
        "a'b'",
        "ERROR",
        &[
            (SyntaxErrorKind::UnquotedName, 0..1),
            (SyntaxErrorKind::UnexpectedToken, 1..4),
        ],
    );
}

#[test]
fn a_run_of_unexpected_characters_is_one_error() {
    assert_outcome(
        "1 + $$.2",
        "(Add 1 2)",
        &[(SyntaxErrorKind::UnexpectedCharacter, 4..7)],
    );
}

#[test]
fn errors_are_listed_in_source_order() {
    assert_eq!(
        errors("* 1 $"),
        [
            (SyntaxErrorKind::ExpectedExpression, 0..0),
            (SyntaxErrorKind::UnexpectedCharacter, 4..5),
        ]
    );
}

#[test]
fn names_may_contain_spaces_japanese_and_symbols() {
    assert_eq!(
        tree("'売上(税抜)' - '値引き/返品 額-1'"),
        "(Subtract '売上(税抜)' '値引き/返品 額-1')"
    );
}

#[test]
fn unicode_whitespace_separates_tokens() {
    assert_eq!(tree("'a'\u{3000}*\u{3000}'b'"), "(Multiply 'a' 'b')");
    assert_eq!(tree("\t1\n+ 2 "), "(Add 1 2)");
}

#[test]
fn numbers_are_integers_or_have_digits_after_the_point() {
    assert_eq!(tree("1"), "1");
    assert_eq!(tree("1.5"), "1.5");
    assert_eq!(tree("10 + 12.25"), "(Add 10 12.25)");
}

#[test]
fn a_point_without_digits_after_it_is_an_unexpected_character() {
    assert_outcome("1.", "1", &[(SyntaxErrorKind::UnexpectedCharacter, 1..2)]);
    assert_outcome(
        "1. + 2",
        "(Add 1 2)",
        &[(SyntaxErrorKind::UnexpectedCharacter, 1..2)],
    );
    assert_outcome(".5", "5", &[(SyntaxErrorKind::UnexpectedCharacter, 0..1)]);
}

#[test]
fn full_width_digits_are_unexpected_characters() {
    assert_outcome(
        "１",
        "ERROR",
        &[
            (SyntaxErrorKind::UnexpectedCharacter, 0..3),
            (SyntaxErrorKind::ExpectedExpression, 3..3),
        ],
    );
}

#[test]
fn a_missing_right_operand_becomes_an_error_node() {
    assert_outcome(
        "'単価' *",
        "(Multiply '単価' ERROR)",
        &[(SyntaxErrorKind::ExpectedExpression, 10..10)],
    );
    let ExpressionKind::Binary { right, .. } = formula::parse("'単価' *").expression.kind else {
        panic!("not a binary expression");
    };
    assert_eq!(right.span, 10..10);
}

#[test]
fn a_missing_left_operand_becomes_an_error_node() {
    assert_outcome(
        "* 2",
        "(Multiply ERROR 2)",
        &[(SyntaxErrorKind::ExpectedExpression, 0..0)],
    );
}

#[test]
fn an_unclosed_parenthesis_keeps_its_inner_expression() {
    assert_outcome(
        "(1 + 2",
        "(Add 1 2)",
        &[(SyntaxErrorKind::ExpectedClosingParenthesis, 6..6)],
    );
}

#[test]
fn an_empty_formula_is_an_error_node() {
    assert_outcome("", "ERROR", &[(SyntaxErrorKind::ExpectedExpression, 0..0)]);
}

#[test]
fn a_lone_closing_parenthesis_is_a_missing_expression_and_a_leftover_token() {
    assert_outcome(
        ")",
        "ERROR",
        &[
            (SyntaxErrorKind::ExpectedExpression, 0..0),
            (SyntaxErrorKind::UnexpectedToken, 0..1),
        ],
    );
}

#[test]
fn an_unterminated_name_runs_to_the_end_of_input() {
    assert_outcome(
        "'単価",
        "'単価'",
        &[(SyntaxErrorKind::UnterminatedName, 0..7)],
    );
    assert_outcome(
        "'a' * 'b",
        "(Multiply 'a' 'b')",
        &[(SyntaxErrorKind::UnterminatedName, 6..8)],
    );
    assert_outcome("'", "ERROR", &[(SyntaxErrorKind::UnterminatedName, 0..1)]);
}

#[test]
fn tokens_after_a_complete_expression_are_reported_together() {
    assert_outcome("1 2", "1", &[(SyntaxErrorKind::UnexpectedToken, 2..3)]);
    assert_outcome("1 2 * 3", "1", &[(SyntaxErrorKind::UnexpectedToken, 2..7)]);
}

#[test]
fn trailing_words_are_unquoted_names_and_trailing_lexer_errors_are_not_repeated() {
    assert_eq!(errors("1abc"), [(SyntaxErrorKind::UnquotedName, 1..4)]);
    assert_eq!(
        errors("単価 数量"),
        [
            (SyntaxErrorKind::UnquotedName, 0..6),
            (SyntaxErrorKind::UnquotedName, 7..13),
        ]
    );
    assert_eq!(
        errors("1 2 abc 3 4"),
        [
            (SyntaxErrorKind::UnexpectedToken, 2..3),
            (SyntaxErrorKind::UnquotedName, 4..7),
            (SyntaxErrorKind::UnexpectedToken, 8..11),
        ]
    );
    assert_eq!(errors("1 '"), [(SyntaxErrorKind::UnterminatedName, 2..3)]);
    assert_eq!(errors("1 ''"), [(SyntaxErrorKind::EmptyName, 2..4)]);
    assert_eq!(
        errors(&format!("1 {}", "9".repeat(400))),
        [(SyntaxErrorKind::NumberOutOfRange, 2..402)]
    );
}

#[test]
fn spans_are_byte_ranges_in_the_source() {
    let binary = formula::parse("1 + 2 * 3").expression;
    assert_eq!(binary.span, 0..9);
    let ExpressionKind::Binary { left, right, .. } = binary.kind else {
        panic!("{:?}", binary.kind);
    };
    assert_eq!(left.span, 0..1);
    assert_eq!(right.span, 4..9);

    assert_eq!(formula::parse("12.5").expression.span, 0..4);
    assert_eq!(formula::parse("'単価'").expression.span, 0..8);
    assert_eq!(formula::parse(" -(1)").expression.span, 1..5);
}

#[test]
fn a_parenthesized_expression_spans_its_parentheses() {
    let parsed = formula::parse("(1 + 2) * 3");
    let ExpressionKind::Binary { left, .. } = parsed.expression.kind else {
        panic!("{:?}", parsed.expression.kind);
    };
    assert_eq!(left.span, 0..7);
    assert!(matches!(left.kind, ExpressionKind::Binary { .. }));

    assert_eq!(formula::parse("(1 + 2").expression.span, 0..6);
}

fn balanced(levels: u32) -> String {
    (0..levels).fold("1".into(), |inner, _| format!("({inner}) + ({inner})"))
}

#[test]
fn negations_parentheses_and_a_chain_mixed_up_to_128_levels_are_accepted() {
    let mixed = format!(
        "{}1{}{}",
        "-(".repeat(32),
        " + 1".repeat(64),
        ")".repeat(32)
    );
    assert_eq!(errors(&mixed), []);
}

#[test]
fn trees_deeper_than_128_levels_are_too_deeply_nested() {
    assert_eq!(
        errors(&format!("{}1{}", "(".repeat(129), ")".repeat(129))),
        [(SyntaxErrorKind::TooDeeplyNested, 128..259)]
    );
    assert_eq!(
        errors(&format!("1 + {}1{}", "(".repeat(128), ")".repeat(128))),
        [(SyntaxErrorKind::TooDeeplyNested, 131..261)]
    );
    assert_eq!(
        errors(&format!("{}1{}", "-".repeat(100), " + 1".repeat(29))),
        [(SyntaxErrorKind::TooDeeplyNested, 214..217)]
    );
    assert_eq!(
        errors(&format!("{}1", "-".repeat(200))),
        [(SyntaxErrorKind::TooDeeplyNested, 128..201)]
    );
    assert_eq!(
        errors(&format!("{}1", "1 + ".repeat(129))),
        [(SyntaxErrorKind::TooDeeplyNested, 514..517)]
    );
    assert_eq!(
        errors(&format!(
            "{}1{}{}",
            "-(".repeat(32),
            " + 1".repeat(65),
            ")".repeat(32)
        )),
        [(SyntaxErrorKind::TooDeeplyNested, 322..357)]
    );
}

#[test]
fn long_but_shallow_formulas_are_accepted() {
    let chain = format!("1{}", " + 1".repeat(100));
    assert_eq!(errors(&format!("({chain}) * ({chain})")), []);
    assert_eq!(errors(&balanced(13)), []);
}

#[test]
fn a_hundred_thousand_open_parentheses_return_instead_of_overflowing_the_stack() {
    let parsed = formula::parse(&"(".repeat(100_000));
    assert_eq!(
        parsed.expression,
        Expression {
            kind: ExpressionKind::Error,
            span: 0..100_000
        }
    );
    assert_eq!(
        parsed.errors,
        [SyntaxError {
            kind: SyntaxErrorKind::TooDeeplyNested,
            span: 128..100_000
        }]
    );
}

#[test]
fn a_hundred_thousand_terms_return_instead_of_overflowing_the_stack() {
    assert_eq!(
        errors(&format!("{}1", "1 + ".repeat(100_000))),
        [(SyntaxErrorKind::TooDeeplyNested, 514..400_001)]
    );
}

#[test]
fn trees_128_levels_deep_parse_print_clone_compare_and_drop_on_a_two_megabyte_stack() {
    let deepest = [
        format!("{}1{}", "(".repeat(128), ")".repeat(128)),
        format!("{}1", "-".repeat(128)),
        format!("1{}", " + 1".repeat(128)),
    ];
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            for source in deepest {
                let parsed = formula::parse(&source);
                assert_eq!(parsed.errors, []);
                assert!(!format!("{parsed:?}").is_empty());
                assert_eq!(parsed.clone(), parsed);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn numbers_too_large_for_f64_are_out_of_range() {
    assert_outcome(
        &format!("1 + {}", "9".repeat(400)),
        "(Add 1 ERROR)",
        &[(SyntaxErrorKind::NumberOutOfRange, 4..404)],
    );
    assert_eq!(errors(&format!("1{}", "0".repeat(308))), []);
    assert_eq!(tree(&format!("0.{}1", "0".repeat(400))), "0");
}

#[test]
fn a_deep_parenthesized_operand_counts_toward_the_depth_of_the_chain_around_it() {
    let source = format!("(1{}){}", " + 1".repeat(127), " * 1".repeat(128));
    assert_eq!(
        errors(&source),
        [(SyntaxErrorKind::TooDeeplyNested, 512..1023)]
    );
}

#[test]
fn sum_without_by_totals_its_one_value() {
    assert_eq!(tree("SUM('売上')"), "(Sum '売上')");
    assert_eq!(
        tree("SUM('単価' * '数量')"),
        "(Sum (Multiply '単価' '数量'))"
    );
}

#[test]
fn sum_by_lists_the_dimensions_and_attributes_to_keep() {
    assert_eq!(
        tree("SUM('売上', BY '期間', 'バージョン')"),
        "(Sum '売上' BY '期間' 'バージョン')"
    );
    assert_eq!(
        tree("SUM('売上', BY '期間', '部門'.'事業部')"),
        "(Sum '売上' BY '期間' '部門'.'事業部')"
    );
    assert_eq!(
        tree("SUM('単価' * '数量', BY '期間')"),
        "(Sum (Multiply '単価' '数量') BY '期間')"
    );
}

#[test]
fn sum_is_an_operand_like_any_other() {
    assert_eq!(
        tree("SUM('a', BY 'b') / SUM('a') * 100"),
        "(Multiply (Divide (Sum 'a' BY 'b') (Sum 'a')) 100)"
    );
    assert_eq!(tree("-SUM('a')"), "(Negate (Sum 'a'))");
    assert_eq!(tree("SUM(SUM('a', BY 'b'))"), "(Sum (Sum 'a' BY 'b'))");
}

#[test]
fn whitespace_may_surround_the_point_between_a_dimension_and_its_attribute() {
    assert_eq!(
        tree("SUM('a', BY 'b' . 'c',\n'd'.'e')"),
        "(Sum 'a' BY 'b'.'c' 'd'.'e')"
    );
}

#[test]
fn sum_and_axis_spans_are_byte_ranges_in_the_source() {
    let sum = formula::parse("SUM('a', BY 'b' . 'c', 'd')").expression;
    assert_eq!(sum.span, 0..27);
    let ExpressionKind::Sum { value, by } = sum.kind else {
        panic!("{:?}", sum.kind);
    };
    assert_eq!(value.span, 4..7);
    let spans: Vec<_> = by.into_iter().map(|axis| axis.span).collect();
    assert_eq!(spans, [12..21, 23..26]);
}

#[test]
fn sum_takes_only_one_value_and_extra_values_are_one_error() {
    assert_outcome(
        "SUM('a', 'b')",
        "(Sum 'a')",
        &[(SyntaxErrorKind::MoreThanOneValue, 9..12)],
    );
    assert_outcome(
        "SUM('a', 'b', 'c', BY 'd')",
        "(Sum 'a' BY 'd')",
        &[(SyntaxErrorKind::MoreThanOneValue, 9..17)],
    );
    assert_outcome(
        "SUM('a',)",
        "(Sum 'a')",
        &[(SyntaxErrorKind::ExpectedExpression, 8..8)],
    );
}

#[test]
fn by_without_axes_is_an_error() {
    assert_outcome(
        "SUM('a', BY)",
        "(Sum 'a')",
        &[(SyntaxErrorKind::EmptyBy, 9..11)],
    );
}

#[test]
fn by_is_written_once() {
    assert_outcome(
        "SUM('a', BY 'b', BY 'c')",
        "(Sum 'a' BY 'b' 'c')",
        &[(SyntaxErrorKind::ByNotLast, 17..19)],
    );
    assert_outcome(
        "SUM(BY 'b', 'a')",
        "(Sum ERROR BY 'b' 'a')",
        &[(SyntaxErrorKind::ByNotLast, 4..6)],
    );
    assert_outcome(
        "SUM('a', BY BY 'b')",
        "(Sum 'a' BY 'b')",
        &[(SyntaxErrorKind::ByNotLast, 12..14)],
    );
    assert_outcome(
        "SUM('a', BY 'b', BY)",
        "(Sum 'a' BY 'b')",
        &[
            (SyntaxErrorKind::ByNotLast, 17..19),
            (SyntaxErrorKind::ExpectedAxis, 19..19),
        ],
    );
}

#[test]
fn an_axis_is_only_a_dimension_or_a_dimension_attribute() {
    for (source, span) in [
        ("SUM('a', BY 'b' + 1)", 12..19),
        ("SUM('a', BY ('b'))", 12..17),
        ("SUM('a', BY ('b', 'c'))", 12..22),
        ("SUM('a', BY 1)", 12..13),
        ("SUM('a', BY 'b'.'c' * 2)", 12..23),
        ("SUM('a', BY SUM('b'))", 12..20),
        ("SUM('a', BY 'b' 'c')", 12..19),
    ] {
        assert_outcome(
            source,
            "(Sum 'a')",
            &[(SyntaxErrorKind::ExpressionInBy, span)],
        );
    }
    assert_outcome(
        "SUM('a', BY 'b' + 1, 'c')",
        "(Sum 'a' BY 'c')",
        &[(SyntaxErrorKind::ExpressionInBy, 12..19)],
    );
}

#[test]
fn an_unquoted_name_in_by_asks_for_quotes() {
    assert_outcome(
        "SUM('a', BY 期間)",
        "(Sum 'a')",
        &[(SyntaxErrorKind::UnquotedName, 12..18)],
    );
    assert_outcome(
        "SUM('a', BY 期間 + 1)",
        "(Sum 'a')",
        &[
            (SyntaxErrorKind::UnquotedName, 12..18),
            (SyntaxErrorKind::ExpressionInBy, 12..22),
        ],
    );
    assert_outcome(
        "SUM('a', BY 'b' c)",
        "(Sum 'a')",
        &[
            (SyntaxErrorKind::ExpressionInBy, 12..17),
            (SyntaxErrorKind::UnquotedName, 16..17),
        ],
    );
}

#[test]
fn a_comma_without_an_axis_after_it_is_an_error() {
    assert_outcome(
        "SUM('a', BY 'b',)",
        "(Sum 'a' BY 'b')",
        &[(SyntaxErrorKind::ExpectedAxis, 16..16)],
    );
    assert_outcome(
        "SUM('a', BY 'b',, 'c')",
        "(Sum 'a' BY 'b' 'c')",
        &[(SyntaxErrorKind::ExpectedAxis, 16..16)],
    );
}

#[test]
fn parentheses_in_by_are_skipped_without_nesting() {
    let source = format!("SUM('a', BY {}", "(".repeat(100_000));
    assert_outcome(
        &source,
        "(Sum 'a')",
        &[
            (SyntaxErrorKind::ExpressionInBy, 12..100_012),
            (
                SyntaxErrorKind::ExpectedClosingParenthesis,
                100_012..100_012,
            ),
        ],
    );
}

#[test]
fn sum_without_parentheses_is_an_error() {
    assert_outcome(
        "SUM 'a'",
        "ERROR",
        &[
            (SyntaxErrorKind::ExpectedOpeningParenthesis, 4..4),
            (SyntaxErrorKind::UnexpectedToken, 4..7),
        ],
    );
}

#[test]
fn an_unclosed_sum_keeps_what_it_read() {
    assert_outcome(
        "SUM('a', BY 'b'",
        "(Sum 'a' BY 'b')",
        &[(SyntaxErrorKind::ExpectedClosingParenthesis, 15..15)],
    );
    assert_eq!(formula::parse("SUM('a', BY 'b'").expression.span, 0..15);
    assert_eq!(formula::parse("SUM('a', BY").expression.span, 0..11);
    assert_eq!(formula::parse("SUM('a', 'b'").expression.span, 0..12);
    assert_eq!(formula::parse("SUM('a', BY 'b' + 1").expression.span, 0..19);
    assert_eq!(formula::parse("SUM( ").expression.span, 0..5);
}

#[test]
fn an_empty_name_is_not_kept_as_an_axis() {
    assert_outcome(
        "SUM('a', BY '', 'b'.'')",
        "(Sum 'a')",
        &[
            (SyntaxErrorKind::EmptyName, 12..14),
            (SyntaxErrorKind::EmptyName, 20..22),
        ],
    );
}

#[test]
fn function_and_keyword_names_are_upper_case() {
    assert_outcome(
        "sum('a')",
        "ERROR",
        &[
            (SyntaxErrorKind::UnquotedName, 0..3),
            (SyntaxErrorKind::UnexpectedToken, 3..8),
        ],
    );
}

#[test]
fn by_and_attributes_outside_sum_are_unexpected() {
    assert_outcome(
        "'a' BY 'b'",
        "'a'",
        &[(SyntaxErrorKind::UnexpectedToken, 4..10)],
    );
    assert_outcome(
        "'a'.'b'",
        "'a'",
        &[(SyntaxErrorKind::UnexpectedToken, 3..7)],
    );
}

#[test]
fn each_sum_is_one_level_of_the_128_level_limit() {
    let nested = |levels: usize| format!("{}1{}", "SUM(".repeat(levels), ")".repeat(levels));
    assert_eq!(errors(&nested(128)), []);
    assert_eq!(
        errors(&nested(129)),
        [(SyntaxErrorKind::TooDeeplyNested, 512..646)]
    );
    assert_eq!(
        errors(&format!("{} + 1", nested(128))),
        [(SyntaxErrorKind::TooDeeplyNested, 642..645)]
    );
    assert_eq!(
        errors(&format!("SUM(1, {}1{})", "(".repeat(128), ")".repeat(128))),
        [(SyntaxErrorKind::TooDeeplyNested, 134..265)]
    );
}
