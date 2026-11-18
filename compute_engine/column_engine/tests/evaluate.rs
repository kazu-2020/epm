use datagen::cases::{self, CASES, Fixture};
use dimension::{AttributeGlobalId, Dimension, Dimensions};
use formula::EvaluateError;
use formula::{CheckedExpression, ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind};
use measure::{Measure, MeasureDefinition};
use ulid::Ulid;

#[test]
fn column_engine_matches_every_hand_computed_case() {
    let fixture = Fixture::default();
    let dimensions = &fixture.dimensions;
    for case in CASES {
        let mut results: Vec<Measure> = Vec::new();
        for &(result, source) in case.steps {
            let measures: Vec<&Measure> = fixture.inputs.iter().chain(&results).collect();
            let measure = column_engine::evaluate(
                &fixture.check(source, result),
                fixture.definition(result),
                &measures,
                dimensions,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            results.push(measure);
        }
        let last = results.last().expect("every case has a step");
        assert_eq!(
            cases::exact(&last.cells().collect::<Vec<_>>()),
            cases::expected(case),
            "{}",
            case.name
        );
    }
}

fn checked(expression: &ResolvedExpression, result: &MeasureDefinition) -> CheckedExpression {
    formula::check_axes(expression, result.axes())
        .checked
        .expect("the changed formula still passes the check")
}

#[test]
fn errors_name_what_the_evaluation_could_not_find() {
    let fixture = Fixture::default();
    let dimensions = &fixture.dimensions;
    let measures: Vec<&Measure> = fixture.inputs.iter().collect();
    let result = fixture.definition("結果");
    let sales = fixture.check("'売上'", "結果");
    assert_eq!(
        column_engine::evaluate(
            &sales,
            fixture.definition("期間結果"),
            &measures,
            dimensions
        )
        .unwrap_err(),
        EvaluateError::ResultAxesChanged(fixture.definition("期間結果").global_id())
    );
    assert_eq!(
        column_engine::evaluate(&sales, result, &[], dimensions).unwrap_err(),
        EvaluateError::MissingMeasure(fixture.definition("売上").global_id())
    );
    let mut stale = fixture.resolve("'売上'");
    let ResolvedExpressionKind::Measure { axes, .. } = &mut stale.kind else {
        panic!("'売上' resolves to a measure");
    };
    axes.reverse();
    assert_eq!(
        column_engine::evaluate(&checked(&stale, result), result, &measures, dimensions)
            .unwrap_err(),
        EvaluateError::MeasureAxesChanged(fixture.definition("売上").global_id())
    );
    for needs_broadcast in ["'売上'", "'売上' + 1"] {
        assert_eq!(
            column_engine::evaluate(
                &fixture.check(needs_broadcast, "結果"),
                result,
                &measures,
                &Dimensions::default()
            )
            .unwrap_err(),
            EvaluateError::MissingDimension(result.axes()[0]),
            "{needs_broadcast}"
        );
    }
    let by_division_source = "SUM('売上', BY '部門'.'事業部', '期間')";
    let division_result = fixture.definition("事業部結果");
    let periods = fixture.dimension("期間");
    let departments = fixture.dimension("部門");
    assert_eq!(
        column_engine::evaluate(
            &fixture.check(by_division_source, "事業部結果"),
            division_result,
            &measures,
            &Fixture::dimensions_without(departments.global_id())
        )
        .unwrap_err(),
        EvaluateError::MissingDimension(departments.global_id())
    );
    let mut unknown_attribute = fixture.resolve(by_division_source);
    let ResolvedExpressionKind::Sum { by, .. } = &mut unknown_attribute.kind else {
        panic!("the formula resolves to a SUM");
    };
    let ResolvedAxisKind::Attribute { attribute, .. } = &mut by[0].kind else {
        panic!("the first BY axis is an attribute");
    };
    *attribute = AttributeGlobalId(Ulid(999));
    assert_eq!(
        column_engine::evaluate(
            &checked(&unknown_attribute, division_result),
            division_result,
            &measures,
            dimensions
        )
        .unwrap_err(),
        EvaluateError::MissingAttribute(AttributeGlobalId(Ulid(999)))
    );
    let one_period = Dimension::try_new(
        periods.definition().clone(),
        periods.batch().slice(0, 1),
        &Dimensions::default(),
    )
    .expect("the first period alone is a dimension");
    let mut with_one_period = Fixture::dimensions_without(periods.global_id());
    with_one_period
        .try_push(one_period)
        .expect("the periods were taken out");
    assert_eq!(
        column_engine::evaluate(&sales, result, &measures, &with_one_period).unwrap_err(),
        EvaluateError::MeasurePackingChanged(fixture.definition("売上").global_id())
    );
}

#[test]
fn a_sum_that_overflows_on_every_cell_leaves_no_cell() {
    let fixture = Fixture::default();
    let dimensions = &fixture.dimensions;
    let measures: Vec<&Measure> = fixture.inputs.iter().collect();

    let huge = format!("1{}", "0".repeat(308));

    let measure = column_engine::evaluate(
        &fixture.check(&format!("'売上' + {huge} + {huge}"), "結果"),
        fixture.definition("結果"),
        &measures,
        dimensions,
    )
    .expect("the formula evaluates");

    assert_eq!(measure.cells().count(), 0);
}
