mod common;

use std::ops::Range;

use formula::{AxisError, AxisErrorKind, CheckedAxis, CheckedNode, ResolveErrorKind, Sign};
use measure::MeasureDefinition;

use AxisErrorKind::{
    AxisNotInResult, DuplicateAxis, ReducesAxesWithoutAggregation, ValueLacksAxis,
};

type Outcome = (
    Option<Vec<String>>,
    Vec<(AxisErrorKind, String, Range<usize>)>,
);

fn resolved(source: &str) -> formula::Resolved {
    let dimensions = common::dimensions_with_members();
    formula::resolve(
        &formula::parse(source).expression,
        &common::measures(),
        &dimensions.iter().collect::<Vec<_>>(),
    )
}

fn resolve_error_kinds(source: &str) -> Vec<ResolveErrorKind> {
    resolved(source)
        .errors
        .into_iter()
        .map(|error| error.kind)
        .collect()
}

fn result(name: &str) -> MeasureDefinition {
    common::measures()
        .into_iter()
        .find(|measure| measure.name == name)
        .expect("the result is a fixture measure")
}

fn check(result: &str, source: &str) -> Outcome {
    let result = self::result(result);
    let check = formula::check_axes(&resolved(source).expression, &result.axes);
    assert_eq!(
        check.checked.is_some(),
        check.broadcast.is_some() && check.errors.is_empty(),
        "{source}"
    );
    if let Some(checked) = &check.checked {
        assert_eq!(checked.result_axes(), result.axes, "{source}");
    }
    let broadcast = check
        .broadcast
        .map(|axes| axes.into_iter().map(common::dimension_name).collect());
    let errors = check
        .errors
        .into_iter()
        .map(
            |AxisError {
                 kind,
                 dimension,
                 span,
             }| (kind, common::dimension_name(dimension), span),
        )
        .collect();
    (broadcast, errors)
}

fn outcome(broadcast: Option<&[&str]>, errors: &[(AxisErrorKind, &str, Range<usize>)]) -> Outcome {
    let broadcast = broadcast.map(|axes| axes.iter().map(|&axis| axis.into()).collect());
    let errors = errors
        .iter()
        .map(|(kind, dimension, span)| (*kind, (*dimension).into(), span.clone()))
        .collect();
    (broadcast, errors)
}

fn passes(broadcast: &[&str]) -> Outcome {
    outcome(Some(broadcast), &[])
}

fn fails_without_broadcast(errors: &[(AxisErrorKind, &str, Range<usize>)]) -> Outcome {
    outcome(Some(&[]), errors)
}

fn unknown(errors: &[(AxisErrorKind, &str, Range<usize>)]) -> Outcome {
    outcome(None, errors)
}

#[test]
fn a_number_has_no_axes_and_broadcasts_in_result_measure_order() {
    assert_eq!(check("売上計画", "1.1"), passes(&["部門", "期間"]));
}

#[test]
fn a_measure_has_the_axes_of_its_definition() {
    assert_eq!(check("売上計画", "'売上'"), passes(&[]));
    assert_eq!(check("売上計画", "'全社売上'"), passes(&["部門"]));
}

#[test]
fn a_negation_has_the_axes_of_its_operand() {
    assert_eq!(check("売上計画", "-'全社売上'"), passes(&["部門"]));
    let source = "-'売上'";
    assert_eq!(
        check("全社売上", source),
        fails_without_broadcast(&[(ReducesAxesWithoutAggregation, "部門", 0..source.len())])
    );
}

#[test]
fn an_arithmetic_operation_has_the_union_of_both_sides() {
    assert_eq!(check("売上計画", "'売上' * 1.1"), passes(&[]));
    assert_eq!(check("売上計画", "'売上' + '全社売上'"), passes(&[]));
    assert_eq!(check("売上計画", "'全社売上' + '売上'"), passes(&[]));
    for source in ["'売上' - '全社売上'", "'全社売上' / '売上'"] {
        assert_eq!(
            check("全社売上", source),
            fails_without_broadcast(&[(ReducesAxesWithoutAggregation, "部門", 0..source.len())]),
            "{source}"
        );
    }
}

#[test]
fn an_expression_with_an_axis_the_result_lacks_reduces_axes_without_aggregation() {
    assert_eq!(check("売上計画", "'全社売上' * 0.1"), passes(&["部門"]));
    let source = "'売上'";
    assert_eq!(
        check("全社売上", source),
        fails_without_broadcast(&[(ReducesAxesWithoutAggregation, "部門", 0..source.len())])
    );
    let source = "'内部取引'";
    assert_eq!(
        check("全社売上", source),
        fails_without_broadcast(&[
            (ReducesAxesWithoutAggregation, "部門", 0..source.len()),
            (ReducesAxesWithoutAggregation, "事業部", 0..source.len()),
        ])
    );
}

#[test]
fn a_division_measure_does_not_lookup_into_a_department_result() {
    let source = "'売上' * '配賦率'";
    assert_eq!(
        check("売上計画", source),
        fails_without_broadcast(&[(ReducesAxesWithoutAggregation, "事業部", 0..source.len())])
    );
}

#[test]
fn a_sum_keeps_only_the_by_axes_with_remapped_axes_replaced_by_their_target() {
    assert_eq!(check("全社売上", "SUM('売上', BY '期間')"), passes(&[]));
    assert_eq!(
        check("事業部売上", "SUM('売上', BY '部門'.'事業部', '期間')"),
        passes(&[])
    );
    assert_eq!(check("売上計画", "SUM('売上')"), passes(&["部門", "期間"]));
}

#[test]
fn a_dimension_twice_in_a_sum_result_is_a_duplicate_axis() {
    let source = "SUM('内部取引', BY '部門'.'事業部', '事業部')";
    assert_eq!(
        check("事業部売上", source),
        unknown(&[(DuplicateAxis, "事業部", common::span(source, "'事業部'"))])
    );
    assert_eq!(
        check("事業部売上", "SUM('内部取引', BY '部門'.'事業部')"),
        passes(&["期間"])
    );
    assert_eq!(
        check("事業部売上", "SUM('内部取引', BY '事業部', '期間')"),
        passes(&[])
    );
}

#[test]
fn a_by_axis_the_value_lacks_is_checked_on_the_dimension_before_remapping() {
    let source = "SUM('売上', BY '期間', 'バージョン')";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(
            ValueLacksAxis,
            "バージョン",
            common::span(source, "'バージョン'")
        )])
    );
    let source = "SUM('事業部売上', BY '部門'.'事業部')";
    assert_eq!(
        check("配賦率", source),
        unknown(&[(
            ValueLacksAxis,
            "部門",
            common::span(source, "'部門'.'事業部'")
        )])
    );
}

#[test]
fn a_by_axis_the_result_lacks_is_checked_on_the_dimension_after_remapping() {
    let source = "SUM('売上', BY '期間', '部門'.'事業部')";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(
            AxisNotInResult,
            "事業部",
            common::span(source, "'部門'.'事業部'")
        )])
    );
    let source = "SUM('売上', BY '部門'.'事業部', '期間')";
    assert_eq!(
        check("売上計画", source),
        unknown(&[(
            AxisNotInResult,
            "事業部",
            common::span(source, "'部門'.'事業部'")
        )])
    );
    let source = "SUM('売上', BY '部門', '期間') + 1";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(AxisNotInResult, "部門", common::span(source, "'部門'"))])
    );
}

#[test]
fn a_sum_inside_another_sums_value_may_keep_axes_the_result_lacks() {
    assert_eq!(
        check("全社売上", "SUM(SUM('売上', BY '部門', '期間'), BY '期間')"),
        passes(&[])
    );
    assert_eq!(
        check(
            "全社売上",
            "SUM(-SUM('売上', BY '部門', '期間') * 2, BY '期間')"
        ),
        passes(&[])
    );
    let source = "SUM(SUM('売上', BY '期間'), BY '部門')";
    assert_eq!(
        check("売上計画", source),
        unknown(&[(ValueLacksAxis, "部門", common::span(source, "'部門'"))])
    );
}

#[test]
fn an_unknown_name_hides_only_the_checks_that_need_its_axes() {
    let source = "('売り上げ' * 1.1) + '全社売上'";
    assert_eq!(resolve_error_kinds(source), [ResolveErrorKind::UnknownName]);
    assert_eq!(check("売上計画", source), unknown(&[]));
    assert_eq!(check("全社売上", "'売り上げ' + '売上'"), unknown(&[]));
    let source = "('売り上げ' * 1.1) + SUM('売上', BY 'バージョン')";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(
            ValueLacksAxis,
            "バージョン",
            common::span(source, "'バージョン'")
        )])
    );
}

#[test]
fn a_sum_of_an_unknown_value_has_unknown_axes_but_still_checks_its_by_against_the_result() {
    assert_eq!(
        check("売上計画", "SUM('売り上げ', BY '部門')"),
        unknown(&[])
    );
    assert_eq!(
        check("全社売上", "SUM('売り上げ', BY '期間') + '売上'"),
        unknown(&[])
    );
    let source = "SUM('売り上げ', BY 'バージョン')";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(
            AxisNotInResult,
            "バージョン",
            common::span(source, "'バージョン'")
        )])
    );
}

#[test]
fn a_sum_with_an_unresolved_by_axis_has_unknown_axes_instead_of_becoming_a_grand_total() {
    let source = "SUM('売上', BY '期間', '部署')";
    assert_eq!(resolve_error_kinds(source), [ResolveErrorKind::UnknownName]);
    assert_eq!(check("全社売上", source), unknown(&[]));
    assert_eq!(
        check("売上計画", "SUM('売上', BY '部門', '期間', '部門'.'地域')"),
        unknown(&[])
    );
    assert_eq!(check("全社売上", "SUM('売上', BY '部署')"), unknown(&[]));
    let source = "SUM('売上', BY 'バージョン', '部署')";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(
            ValueLacksAxis,
            "バージョン",
            common::span(source, "'バージョン'")
        )])
    );
}

#[test]
fn a_syntax_error_node_has_unknown_axes() {
    let source = "'売上' +";
    assert!(!formula::parse(source).errors.is_empty());
    assert_eq!(check("全社売上", source), unknown(&[]));
}

#[test]
fn a_by_axis_error_makes_the_sum_axes_unknown_so_an_outer_sum_does_not_repeat_it() {
    let source = "SUM(SUM('全社売上', BY '部門', '期間'), BY '部門', '期間')";
    let inner = source.find("'部門'").expect("the inner BY names 部門");
    assert_eq!(
        check("売上計画", source),
        unknown(&[(ValueLacksAxis, "部門", inner..inner + "'部門'".len())])
    );
}

fn render(node: &CheckedNode) -> String {
    match node {
        CheckedNode::Number(value) => value.to_string(),
        CheckedNode::Measure { axes, .. } => {
            let axes: Vec<_> = axes.iter().copied().map(common::dimension_name).collect();
            format!("[{}]", axes.join(" "))
        }
        CheckedNode::Terms(terms) => {
            let terms: Vec<_> = terms
                .iter()
                .map(|term| match term.sign {
                    Sign::Plus => format!("+{}", render(&term.node)),
                    Sign::Minus => format!("-{}", render(&term.node)),
                })
                .collect();
            format!("(Terms {})", terms.join(" "))
        }
        CheckedNode::Product {
            operator,
            left,
            right,
        } => format!("({operator:?} {} {})", render(left), render(right)),
        CheckedNode::Sum { value, by } => {
            let axes: Vec<_> = by
                .iter()
                .map(|axis| match *axis {
                    CheckedAxis::Dimension(dimension) => common::dimension_name(dimension),
                    CheckedAxis::Attribute {
                        dimension, target, ..
                    } => format!(
                        "{}->{}",
                        common::dimension_name(dimension),
                        common::dimension_name(target)
                    ),
                })
                .collect();
            format!("(Sum {} BY {})", render(value), axes.join(" "))
        }
        CheckedNode::Filter {
            value,
            dimension,
            members,
            keeps_axis,
        } => format!(
            "(Filter {} {}: {members:?} keeps_axis={keeps_axis})",
            render(value),
            common::dimension_name(*dimension)
        ),
    }
}

#[test]
fn a_passing_check_keeps_the_resolved_tree_without_spans() {
    let check = formula::check_axes(
        &resolved("-SUM('売上' / 2, BY '部門'.'事業部', '期間') - 1").expression,
        &result("事業部売上").axes,
    );
    assert_eq!(check.errors, []);
    assert_eq!(
        render(check.checked.expect("the check passes").root()),
        "(Terms -(Sum (Divide [部門 期間] 2) BY 部門->事業部 期間) -1)"
    );
}

#[test]
fn a_chain_of_additions_and_subtractions_splits_into_signed_terms_through_parentheses_and_negation()
{
    let check = formula::check_axes(
        &resolved("'売上' - (2 - -'売上') + 1 * 3").expression,
        &result("売上").axes,
    );
    assert_eq!(check.errors, []);
    let checked = check.checked.expect("the check passes");
    assert_eq!(
        render(checked.root()),
        "(Terms +[部門 期間] -2 -[部門 期間] +(Multiply 1 3))"
    );
}

#[test]
fn a_bracket_removes_its_axis_and_filter_keeps_it() {
    assert_eq!(check("全社売上", "'売上'['部門': '営業']"), passes(&[]));
    let source = "FILTER('売上', '部門' = '営業')";
    assert_eq!(
        check("全社売上", source),
        outcome(
            Some(&[]),
            &[(ReducesAxesWithoutAggregation, "部門", 0..source.len())]
        )
    );
    assert_eq!(check("売上計画", source), passes(&[]));
    let check = formula::check_axes(
        &resolved(
            "'売上'['部門': '開発'] + FILTER('売上'['期間': '1月'], '部門' IN ('営業', '開発'))",
        )
        .expression,
        &result("売上計画").axes,
    );
    assert_eq!(
        render(check.checked.expect("the check passes").root()),
        "(Terms +(Filter [部門 期間] 部門: [MemberId(1)] keeps_axis=false) +(Filter (Filter [部門 期間] 期間: [MemberId(0)] keeps_axis=false) 部門: [MemberId(0), MemberId(1)] keeps_axis=true))"
    );
}

#[test]
fn a_filter_needs_its_dimension_on_the_value() {
    let source = "'全社売上'['部門': '営業']";
    assert_eq!(
        check("全社売上", source),
        unknown(&[(ValueLacksAxis, "部門", 0..source.len())])
    );
}
