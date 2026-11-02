use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, UInt32Type};
use datagen::Configuration;
use dimension::{Dimension, DimensionDefinition, MemberId};
use measure::{Measure, MeasureDefinition};
use oracle::Oracle;
use proptest::prelude::*;

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

fn rows(measure: &Measure) -> Vec<(Vec<MemberId>, f64)> {
    let columns = measure.base().columns();
    let (coordinate_columns, value_column) = columns.split_at(columns.len() - 1);
    let values = value_column[0].as_primitive::<Float64Type>();
    (0..values.len())
        .map(|row| {
            let coordinate = coordinate_columns
                .iter()
                .map(|column| MemberId(column.as_primitive::<UInt32Type>().value(row)))
                .collect();
            (coordinate, values.value(row))
        })
        .collect()
}

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-9 * left.abs().max(right.abs()) + 1e-6
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn evaluator_agrees_with_the_oracle(configuration in configurations()) {
        let workspace = datagen::generate(&configuration);
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let dimension_definitions: Vec<DimensionDefinition> = dimensions
            .iter()
            .map(|dimension| dimension.definition().clone())
            .collect();
        let measure_definitions: Vec<MeasureDefinition> = workspace
            .inputs
            .iter()
            .map(|input| input.definition().clone())
            .chain(workspace.formulas.iter().map(|(result, _)| result.clone()))
            .collect();
        let oracle = Oracle::new(&dimensions, &workspace.inputs.iter().collect::<Vec<_>>())
            .expect("the oracle loads the workspace");
        let mut measures: Vec<Measure> = workspace.inputs.clone();
        for (result, text) in &workspace.formulas {
            let parsed = formula::parse(text);
            prop_assert_eq!(&parsed.errors, &[], "{}", text);
            let resolved =
                formula::resolve(&parsed.expression, &measure_definitions, &dimension_definitions);
            prop_assert_eq!(&resolved.errors, &[], "{}", text);
            let check = formula::check_axes(&resolved.expression, &result.axes);
            prop_assert_eq!(&check.errors, &[], "{}", text);

            let actual = evaluator::evaluate(
                &resolved.expression,
                result,
                &measures.iter().collect::<Vec<_>>(),
                &dimensions,
            )
            .expect("the evaluator evaluates a checked formula");
            let expected = oracle
                .evaluate(&resolved.expression, result)
                .expect("the oracle evaluates a checked formula");
            let actual_rows = rows(&actual);
            let coordinates = |rows: &[(Vec<MemberId>, f64)]| {
                rows.iter().map(|(coordinate, _)| coordinate.clone()).collect::<Vec<_>>()
            };
            prop_assert_eq!(
                coordinates(&actual_rows),
                coordinates(&expected),
                "{} = {} with {:?}",
                result.name,
                text,
                configuration
            );
            for ((coordinate, left), (_, right)) in actual_rows.iter().zip(&expected) {
                prop_assert!(
                    close(*left, *right),
                    "{} = {} at {:?}: evaluator {} oracle {} with {:?}",
                    result.name,
                    text,
                    coordinate,
                    left,
                    right,
                    configuration
                );
            }
            measures.push(actual);
        }
    }
}
