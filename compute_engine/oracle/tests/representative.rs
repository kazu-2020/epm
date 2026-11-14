use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::Dimension;
use measure::{Measure, MeasureDefinition};
use oracle::Oracle;

const SMALL: Size = Size {
    products: 30,
    customers: 20,
    departments_per_kind: 4,
    quantity_cells: 3_000,
    account_fill: 0.5,
};

#[test]
fn every_engine_computes_the_small_representative_model_bit_for_bit_alike() {
    let workspace = representative::generate(&SMALL, 7);
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
    let mut oracle = Oracle::new(&dimensions, &workspace.inputs.iter().collect::<Vec<_>>())
        .expect("the oracle loads the workspace");
    let mut measures: Vec<Measure> = workspace.inputs.clone();
    for formula in &workspace.formulas {
        let checked = workspace::compile(&formula.text, &formula.result, &definitions, &dimensions)
            .unwrap_or_else(|error| panic!("{}: {error:?}", formula.text));
        let references: Vec<&Measure> = measures.iter().collect();
        let evaluated = evaluator::evaluate(&checked, &formula.result, &references, &dimensions)
            .expect("the evaluator evaluates the formula");
        let column = column_engine::evaluate(&checked, &formula.result, &references, &dimensions)
            .expect("the column engine evaluates the formula");
        let expected = exact(
            &oracle
                .evaluate(&checked, formula.result.global_id)
                .expect("the oracle evaluates the formula"),
        );
        assert!(!expected.is_empty(), "{} has cells", formula.result.name);
        assert_eq!(
            exact(&evaluated.cells().collect::<Vec<_>>()),
            expected,
            "evaluator: {}",
            formula.result.name
        );
        assert_eq!(
            exact(&column.cells().collect::<Vec<_>>()),
            expected,
            "column engine: {}",
            formula.result.name
        );
        measures.push(column);
    }
}
