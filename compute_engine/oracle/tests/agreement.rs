use datagen::Configuration;
use datagen::cases::exact;
use dimension::Dimension;
use measure::{Measure, MeasureDefinition};
use oracle::Oracle;
use proptest::prelude::*;
use workspace::Formula;

fn configurations() -> impl Strategy<Value = Configuration> {
    (
        any::<u64>(),
        1..=4usize,
        1..=8u32,
        1..=4usize,
        0..=30usize,
        0.0..=2.0f64,
        1..=5usize,
        0..=4usize,
    )
        .prop_map(
            |(
                seed,
                dimension_count,
                members_per_dimension,
                input_count,
                cells_per_input,
                zipf_exponent,
                chain_length,
                maximum_depth,
            )| Configuration {
                seed,
                dimension_count,
                members_per_dimension,
                input_count,
                cells_per_input,
                zipf_exponent,
                chain_length,
                maximum_depth,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn evaluator_and_column_engine_agree_with_the_oracle(configuration in configurations()) {
        let workspace = datagen::generate(&configuration);
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let measure_definitions: Vec<MeasureDefinition> = workspace
            .inputs
            .iter()
            .map(|input| input.definition().clone())
            .chain(workspace.formulas.iter().map(|formula| formula.result.clone()))
            .collect();
        let mut oracle = Oracle::new(&dimensions, &workspace.inputs.iter().collect::<Vec<_>>())
            .expect("the oracle loads the workspace");
        let mut measures: Vec<Measure> = workspace.inputs.clone();
        for Formula { result, text } in &workspace.formulas {
            let compiled = workspace::compile(text, result, &measure_definitions, &dimensions);
            prop_assert!(compiled.is_ok(), "{}: {:?}", text, compiled.err());
            let checked = compiled.expect("the formula compiles");

            let actual = evaluator::evaluate(
                &checked,
                result,
                &measures.iter().collect::<Vec<_>>(),
                &dimensions,
            )
            .expect("the evaluator evaluates a checked formula");
            let column = column_engine::evaluate(
                &checked,
                result,
                &measures.iter().collect::<Vec<_>>(),
                &dimensions,
            )
            .expect("the column engine evaluates a checked formula");
            let expected = oracle
                .evaluate(&checked, result.global_id)
                .expect("the oracle evaluates a checked formula");
            let actual_cells: Vec<_> = actual.cells().collect();
            prop_assert_eq!(
                exact(&actual_cells),
                exact(&expected),
                "{} = {} with {:?}",
                result.name,
                text,
                configuration
            );
            let column_cells: Vec<_> = column.cells().collect();
            prop_assert_eq!(
                exact(&column_cells),
                exact(&expected),
                "column engine: {} = {} with {:?}",
                result.name,
                text,
                configuration
            );
            measures.push(actual);
        }
    }
}
