mod common;

use std::ops::Range;

use formula::{
    ResolveError, ResolveErrorKind, ResolvedAxis, ResolvedAxisKind, ResolvedExpression,
    ResolvedExpressionKind,
};

fn render(expression: &ResolvedExpression) -> String {
    match &expression.kind {
        ResolvedExpressionKind::Number(value) => value.to_string(),
        ResolvedExpressionKind::Tile { ulid, axes } => {
            let tile = common::tiles()
                .into_iter()
                .find(|tile| tile.ulid == *ulid)
                .expect("resolution points at a fixture tile");
            let axes: Vec<_> = axes.iter().copied().map(common::dimension_name).collect();
            format!("'{}'{{{}}}", tile.name, axes.join(","))
        }
        ResolvedExpressionKind::Negate(operand) => format!("(Negate {})", render(operand)),
        ResolvedExpressionKind::Binary {
            operator,
            left,
            right,
        } => format!("({operator:?} {} {})", render(left), render(right)),
        ResolvedExpressionKind::Sum { value, by } => {
            let axes: Vec<_> = by.iter().map(render_axis).collect();
            format!("(Sum {} BY [{}])", render(value), axes.join(" "))
        }
        ResolvedExpressionKind::Error => "ERROR".into(),
    }
}

fn render_axis(axis: &ResolvedAxis) -> String {
    match axis.kind {
        ResolvedAxisKind::Dimension(dimension) => {
            format!("'{}'", common::dimension_name(dimension))
        }
        ResolvedAxisKind::Attribute {
            dimension,
            attribute,
            target,
        } => {
            let attribute = common::dimensions()
                .into_iter()
                .flat_map(|dimension| dimension.attributes)
                .find(|candidate| candidate.ulid == attribute)
                .expect("resolution points at a fixture attribute");
            format!(
                "'{}'.'{}'->'{}'",
                common::dimension_name(dimension),
                attribute.name,
                common::dimension_name(target)
            )
        }
        ResolvedAxisKind::Unresolved => "UNRESOLVED".into(),
    }
}

fn resolve(source: &str) -> (String, Vec<(ResolveErrorKind, Range<usize>)>) {
    let parsed = formula::parse(source);
    let resolved = formula::resolve(&parsed.expression, &common::tiles(), &common::dimensions());
    let errors = resolved
        .errors
        .into_iter()
        .map(|ResolveError { kind, span }| (kind, span))
        .collect();
    (render(&resolved.expression), errors)
}

#[test]
fn a_tile_name_resolves_to_the_tile_and_its_axes() {
    assert_eq!(
        resolve("'売上' * 1.1"),
        ("(Multiply '売上'{部門,期間} 1.1)".into(), vec![])
    );
    assert_eq!(
        resolve("-'全社売上'"),
        ("(Negate '全社売上'{期間})".into(), vec![])
    );
}

#[test]
fn by_resolves_a_dimension_and_an_attribute_with_its_target() {
    assert_eq!(
        resolve("SUM('売上', BY '部門'.'事業部', '期間')"),
        (
            "(Sum '売上'{部門,期間} BY ['部門'.'事業部'->'事業部' '期間'])".into(),
            vec![]
        )
    );
}

#[test]
fn an_undefined_name_is_an_unknown_name_at_its_span() {
    let source = "'売り上げ' * 1.1";
    assert_eq!(
        resolve(source),
        (
            "(Multiply ERROR 1.1)".into(),
            vec![(
                ResolveErrorKind::UnknownName,
                common::span(source, "'売り上げ'")
            )]
        )
    );
}

#[test]
fn names_match_exactly_without_normalization() {
    assert_eq!(resolve("'uriage'"), ("'uriage'{期間}".into(), vec![]));
    for source in ["'売上 '", "'ｕｒｉａｇｅ'", "'URIAGE'"] {
        assert_eq!(
            resolve(source),
            (
                "ERROR".into(),
                vec![(ResolveErrorKind::UnknownName, 0..source.len())]
            ),
            "{source}"
        );
    }
}

#[test]
fn a_dimension_written_as_a_value_is_its_own_error() {
    let source = "'売上' * '期間'";
    assert_eq!(
        resolve(source),
        (
            "(Multiply '売上'{部門,期間} ERROR)".into(),
            vec![(
                ResolveErrorKind::DimensionAsValue,
                common::span(source, "'期間'")
            )]
        )
    );
    let source = "SUM('部門', BY '期間')";
    assert_eq!(
        resolve(source),
        (
            "(Sum ERROR BY ['期間'])".into(),
            vec![(
                ResolveErrorKind::DimensionAsValue,
                common::span(source, "'部門'")
            )]
        )
    );
}

#[test]
fn a_name_that_is_neither_a_tile_nor_a_dimension_is_unknown_not_a_dimension_as_value() {
    let source = "'売上' * '売り上げ'";
    assert_eq!(
        resolve(source).1,
        [(
            ResolveErrorKind::UnknownName,
            common::span(source, "'売り上げ'")
        )]
    );
}

#[test]
fn a_by_axis_must_name_a_dimension_and_a_tile_there_is_its_own_error() {
    for (source, axis, kind) in [
        (
            "SUM('売上', BY '期間', '部署')",
            "'部署'",
            ResolveErrorKind::UnknownName,
        ),
        (
            "SUM('売上', BY '期間', '売上')",
            "'売上'",
            ResolveErrorKind::TileAsAxis,
        ),
    ] {
        assert_eq!(
            resolve(source),
            (
                "(Sum '売上'{部門,期間} BY ['期間' UNRESOLVED])".into(),
                vec![(kind, common::span(source, axis))]
            ),
            "{source}"
        );
    }
}

#[test]
fn a_by_attribute_must_belong_to_its_dimension() {
    for (source, axis, tree) in [
        (
            "SUM('売上', BY '部門'.'地域', '期間')",
            "'部門'.'地域'",
            "(Sum '売上'{部門,期間} BY [UNRESOLVED '期間'])",
        ),
        (
            "SUM('売上', BY '部門', '期間'.'事業部')",
            "'期間'.'事業部'",
            "(Sum '売上'{部門,期間} BY ['部門' UNRESOLVED])",
        ),
    ] {
        assert_eq!(
            resolve(source),
            (
                tree.into(),
                vec![(
                    ResolveErrorKind::UnknownAttribute,
                    common::span(source, axis)
                )]
            ),
            "{source}"
        );
    }
}

#[test]
fn errors_from_every_part_of_the_expression_are_returned_in_source_order() {
    let source = "'売り上げ' + SUM('期間', BY '部署')";
    assert_eq!(
        resolve(source).1,
        [
            (
                ResolveErrorKind::UnknownName,
                common::span(source, "'売り上げ'")
            ),
            (
                ResolveErrorKind::DimensionAsValue,
                common::span(source, "'期間'")
            ),
            (
                ResolveErrorKind::UnknownName,
                common::span(source, "'部署'")
            ),
        ]
    );
}

#[test]
fn a_syntax_error_node_resolves_to_an_error_node_without_a_new_error() {
    assert_eq!(
        resolve("'売上' +"),
        ("(Add '売上'{部門,期間} ERROR)".into(), vec![])
    );
}
