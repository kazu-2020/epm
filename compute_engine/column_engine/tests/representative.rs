use datagen::Formula;
use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::Dimension;
use measure::{Measure, MeasureDefinition, MeasureUlid};
use ulid::Ulid;

#[test]
fn small_model_matches_the_evaluator_on_dense_paths() {
    matches_the_evaluator(
        &[],
        &Size {
            products: 30,
            customers: 20,
            departments_per_kind: 4,
            quantity_cells: 3_000,
            account_fill: 0.5,
        },
    );
}

#[test]
fn wide_model_matches_the_evaluator_on_sparse_paths() {
    matches_the_evaluator(
        &[
            "'売上' + SUM('売上', BY '部門', '商品', '取引先', '期間')",
            "'売上' * SUM('売上', BY '部門', '商品', '取引先', '期間')",
        ],
        &Size {
            products: 2_000,
            customers: 1_000,
            departments_per_kind: 4,
            quantity_cells: 3_000,
            account_fill: 0.5,
        },
    );
}

fn matches_the_evaluator(extra_formulas: &[&str], size: &Size) {
    let mut workspace = representative::generate(size, 7);
    let sales_axes = workspace.formulas[0].result.axes.clone();
    for (index, text) in extra_formulas.iter().enumerate() {
        workspace.formulas.push(Formula {
            result: MeasureDefinition {
                ulid: MeasureUlid(Ulid(300 + index as u128)),
                name: text.to_string(),
                axes: sales_axes.clone(),
            },
            text: text.to_string(),
        });
    }
    let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
    let definitions: Vec<MeasureDefinition> = workspace
        .inputs
        .iter()
        .map(|input| input.definition().clone())
        .chain(
            workspace
                .formulas
                .iter()
                .map(|formula| formula.result.clone()),
        )
        .collect();
    let mut measures: Vec<Measure> = workspace.inputs.clone();
    for formula in &workspace.formulas {
        let parsed = formula::parse(&formula.text);
        let resolved = formula::resolve(&parsed.expression, &definitions, &dimensions);
        let checked = formula::check_axes(&resolved.expression, &formula.result.axes)
            .checked
            .expect("the representative formulas are checked");
        let references: Vec<&Measure> = measures.iter().collect();
        let expected = evaluator::evaluate(&checked, &formula.result, &references, &dimensions)
            .expect("the evaluator evaluates the formula");
        let column = column_engine::evaluate(&checked, &formula.result, &references, &dimensions)
            .expect("the column engine evaluates the formula");
        let expected = exact(&expected.cells().collect::<Vec<_>>());
        assert!(!expected.is_empty(), "{} has cells", formula.result.name);
        assert_eq!(
            exact(&column.cells().collect::<Vec<_>>()),
            expected,
            "{}",
            formula.result.name
        );
        measures.push(column);
    }
}
