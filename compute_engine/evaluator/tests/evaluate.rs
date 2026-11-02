use datagen::cases::{self, CASES, Fixture};
use dimension::{AttributeUlid, Dimension};
use evaluator::EvaluateError;
use formula::{ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind};
use measure::{Measure, MeasureDefinition, MeasureError, MeasureUlid};
use ulid::Ulid;

#[test]
fn evaluator_matches_every_hand_computed_case() {
    let fixture = Fixture::default();
    let dimensions: Vec<&Dimension> = fixture.dimensions.iter().collect();
    for case in CASES {
        let mut results: Vec<Measure> = Vec::new();
        for &(result, source) in case.steps {
            let measures: Vec<&Measure> = fixture.inputs.iter().chain(&results).collect();
            let measure = evaluator::evaluate(
                &fixture.resolve(source),
                fixture.definition(result),
                &measures,
                &dimensions,
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

#[test]
fn errors_name_what_the_evaluation_could_not_find() {
    let fixture = Fixture::default();
    let dimensions: Vec<&Dimension> = fixture.dimensions.iter().collect();
    let measures: Vec<&Measure> = fixture.inputs.iter().collect();
    let result = fixture.definition("結果");
    let sales = fixture.resolve("'売上'");
    let unresolved = ResolvedExpression {
        kind: ResolvedExpressionKind::Error,
        span: 0..0,
    };
    assert_eq!(
        evaluator::evaluate(&unresolved, result, &measures, &dimensions).unwrap_err(),
        EvaluateError::Unresolved
    );
    assert!(matches!(
        evaluator::evaluate(&sales, fixture.definition("期間結果"), &measures, &dimensions),
        Err(EvaluateError::Axes(errors)) if errors.len() == 1
    ));
    assert_eq!(
        evaluator::evaluate(&sales, result, &[], &dimensions).unwrap_err(),
        EvaluateError::MissingMeasure(fixture.definition("売上").ulid)
    );
    let mut stale = sales.clone();
    let ResolvedExpressionKind::Measure { axes, .. } = &mut stale.kind else {
        panic!("'売上' resolves to a measure");
    };
    axes.reverse();
    assert_eq!(
        evaluator::evaluate(&stale, result, &measures, &dimensions).unwrap_err(),
        EvaluateError::MeasureAxesChanged(fixture.definition("売上").ulid)
    );
    for needs_broadcast in ["'売上'", "'売上' + 1"] {
        assert_eq!(
            evaluator::evaluate(&fixture.resolve(needs_broadcast), result, &measures, &[])
                .unwrap_err(),
            EvaluateError::MissingDimension(result.axes[0]),
            "{needs_broadcast}"
        );
    }
    let by_division = fixture.resolve("SUM('売上', BY '部門'.'事業部', '期間')");
    let division_result = fixture.definition("事業部結果");
    let [periods, departments, divisions] = &fixture.dimensions[..] else {
        panic!("the fixture has three dimensions");
    };
    assert_eq!(
        evaluator::evaluate(
            &by_division,
            division_result,
            &measures,
            &[periods, divisions]
        )
        .unwrap_err(),
        EvaluateError::MissingDimension(departments.ulid())
    );
    let mut unknown_attribute = by_division.clone();
    let ResolvedExpressionKind::Sum { by, .. } = &mut unknown_attribute.kind else {
        panic!("the formula resolves to a SUM");
    };
    let ResolvedAxisKind::Attribute { attribute, .. } = &mut by[0].kind else {
        panic!("the first BY axis is an attribute");
    };
    *attribute = AttributeUlid(Ulid(999));
    assert_eq!(
        evaluator::evaluate(&unknown_attribute, division_result, &measures, &dimensions)
            .unwrap_err(),
        EvaluateError::MissingAttribute(AttributeUlid(Ulid(999)))
    );
    let without_axes = MeasureDefinition {
        ulid: MeasureUlid(Ulid(99)),
        name: "軸なし".into(),
        axes: Vec::new(),
    };
    assert_eq!(
        evaluator::evaluate(&fixture.resolve("1"), &without_axes, &measures, &dimensions)
            .unwrap_err(),
        EvaluateError::InvalidResult(MeasureError::NoAxes)
    );
}
