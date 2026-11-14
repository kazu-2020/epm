use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::Dimension;
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use ulid::Ulid;
use workspace::Formula;

#[test]
fn small_model_matches_the_evaluator_on_dense_paths() {
    matches_the_evaluator(
        &["SUM('売上', BY '部門', '期間') * SUM('売上', BY '商品', '期間')"],
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

#[test]
fn repeated_subexpressions_match_the_evaluator() {
    matches_the_evaluator(
        &[
            "SUM('売上' * 2, BY '部門', '期間') / SUM('売上' * 2, BY '期間') + '売上' * 2",
            "FILTER('売上' * 2, 'バージョン' = '実績') - FILTER('売上' * 2, 'バージョン' = '予算') + '売上' * 2",
            "FILTER('売上' * 2 + 1, 'バージョン' = '実績') - FILTER('売上' * 2 + 1, 'バージョン' = '予算') + '売上' * 2",
        ],
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
fn model_larger_than_one_parallel_piece_matches_the_evaluator_on_one_and_four_threads() {
    for threads in [1, 4] {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("a thread pool starts")
            .install(|| {
                matches_the_evaluator(
                    &["SUM('売上', BY '部門', '期間') * SUM('売上', BY '商品', '期間')"],
                    &Size {
                        products: 30,
                        customers: 20,
                        departments_per_kind: 4,
                        quantity_cells: 40_000,
                        account_fill: 0.5,
                    },
                );
            });
    }
}

fn matches_the_evaluator(extra_formulas: &[&str], size: &Size) {
    let mut workspace = representative::generate(size, 7);
    let sales_axes = workspace.formulas[0].result.axes.clone();
    for (index, text) in extra_formulas.iter().enumerate() {
        workspace.formulas.push(Formula {
            result: MeasureDefinition {
                ulid: MeasureGlobalId(Ulid(300 + index as u128)),
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
        let checked = workspace::compile(&formula.text, &formula.result, &definitions, &dimensions)
            .unwrap_or_else(|error| panic!("{}: {error:?}", formula.text));
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
