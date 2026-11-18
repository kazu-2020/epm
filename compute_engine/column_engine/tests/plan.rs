use datagen::numbered_dimension;
use dimension::{Dimension, DimensionGlobalId, DimensionKind, Dimensions};
use formula::{CheckedExpression, EvaluateError};
use measure::{MeasureDefinition, MeasureGlobalId};
use ulid::Ulid;

fn axes(ids: impl IntoIterator<Item = u128>) -> Vec<DimensionGlobalId> {
    ids.into_iter()
        .map(|id| DimensionGlobalId(Ulid(id)))
        .collect()
}

fn definition(id: u128, name: &str, ids: &[u128]) -> MeasureDefinition {
    MeasureDefinition::try_new(
        MeasureGlobalId(Ulid(id)),
        name.into(),
        axes(ids.iter().copied()),
    )
    .expect("the axes are distinct and not empty")
}

fn plan(
    text: &str,
    result: &MeasureDefinition,
    measures: &[MeasureDefinition],
    dimensions: &Dimensions,
) -> Result<(), EvaluateError> {
    let resolved = formula::resolve(&formula::parse(text).expression, measures, dimensions);
    let checked: CheckedExpression = formula::check_axes(&resolved.expression, result.axes())
        .checked
        .expect("the formula passes the axis check");
    column_engine::plan(&checked, result, dimensions)
}

fn ordinary(id: u128, members: u32) -> Dimension {
    numbered_dimension(id, members, DimensionKind::Ordinary, &Dimensions::default())
}

#[test]
fn a_plan_reports_a_product_or_terms_in_the_middle_wider_than_64_bits_without_cells() {
    let dimensions = Dimensions::try_new((1..=5).map(|id| ordinary(id, 1 << 13)).collect())
        .expect("each dimension has its own ULID");
    let result = definition(21, "結果", &[3]);
    let measures = [
        definition(11, "左", &[1, 2, 3]),
        definition(12, "右", &[3, 4, 5]),
        definition(13, "未参照", &[99]),
        definition(14, "広", &[1, 2, 3, 4, 5]),
    ];

    assert_eq!(
        plan("SUM('左' * '右', BY 'D3')", &result, &measures, &dimensions),
        Err(EvaluateError::PackingTooWide(axes(1..=5)))
    );
    assert_eq!(
        plan("SUM('左' + '右', BY 'D3')", &result, &measures, &dimensions),
        Err(EvaluateError::PackingTooWide(axes(1..=5))),
        "the terms are spread to the union of their axes"
    );
    assert_eq!(
        plan(
            "'右' * SUM('左' * '右', BY 'D3')",
            &definition(23, "右の結果", &[3, 4, 5]),
            &measures,
            &dimensions
        ),
        Err(EvaluateError::PackingTooWide(axes(1..=5))),
        "an operand with no cells does not stop the evaluation of the other operand"
    );
    let wide_result = definition(22, "広い結果", &[1, 2, 3, 4, 5]);
    for text in ["0", "SUM(1 + '左' + '右', BY 'D3')"] {
        assert_eq!(
            plan(text, &wide_result, &measures, &dimensions)
                .map_err(|error| matches!(error, EvaluateError::PackingTooWide(_))),
            Err(true),
            "a number spread to more than 64 bits is reported before its cells are copied: {text}"
        );
    }
    assert_eq!(
        plan(
            "'広'",
            &definition(22, "広い結果", &[1, 2, 3, 4, 5]),
            &measures,
            &dimensions
        ),
        Err(EvaluateError::PackingTooWide(axes(1..=5))),
        "a measure the formula reads names its own axes"
    );
    assert_eq!(
        plan(
            "SUM('左', BY 'D3') * SUM('右', BY 'D3')",
            &result,
            &measures,
            &dimensions
        ),
        Ok(()),
        "every result in the middle has 13 bits, and a measure the formula does not use is not built"
    );
}

#[test]
fn a_plan_reports_a_sum_in_the_middle_wider_than_its_value_without_cells() {
    let mut dimensions = Dimensions::try_new((2..=6).map(|id| ordinary(id, 1 << 13)).collect())
        .expect("each dimension has its own ULID");
    let source = numbered_dimension(1, 4, DimensionKind::Ordinary, &dimensions);
    dimensions
        .try_push(source)
        .expect("the source has its own ULID");
    let result = definition(21, "結果", &[2]);
    let measures = [definition(11, "値", &[1])];

    assert_eq!(
        plan(
            "SUM(SUM('値', BY 'D1'.'D2', 'D1'.'D3', 'D1'.'D4', 'D1'.'D5', 'D1'.'D6'), BY 'D2')",
            &result,
            &measures,
            &dimensions
        ),
        Err(EvaluateError::PackingTooWide(axes(2..=6))),
        "five targets of 13 bits each take 65 bits, while the value takes 2"
    );
    assert_eq!(
        plan(
            "SUM(SUM('値', BY 'D1'.'D2', 'D1'.'D3'), BY 'D2')",
            &result,
            &measures,
            &dimensions
        ),
        Ok(())
    );
}
