use std::collections::HashMap;

use arrow_array::RecordBatch;
use arrow_array::cast::AsArray;
use arrow_array::types::UInt32Type;
use datagen::{Configuration, Workspace};
use dimension::DimensionDefinition;
use measure::MeasureDefinition;

fn configuration(seed: u64) -> Configuration {
    Configuration {
        seed,
        dimension_count: 4,
        members_per_dimension: 8,
        input_count: 3,
        cells_per_input: 40,
        zipf_exponent: 1.1,
        chain_length: 5,
        maximum_depth: 4,
    }
}

fn batches(workspace: &Workspace) -> Vec<RecordBatch> {
    let dimensions = workspace
        .dimensions
        .iter()
        .map(|dimension| dimension.batch());
    let inputs = workspace.inputs.iter().map(|input| input.base());
    dimensions.chain(inputs).cloned().collect()
}

#[test]
fn the_same_seed_generates_the_same_workspace() {
    let first = datagen::generate(&configuration(7));
    let second = datagen::generate(&configuration(7));
    let other = datagen::generate(&configuration(8));
    assert_eq!(batches(&first), batches(&second));
    assert_eq!(first.formulas, second.formulas);
    assert_ne!(first.formulas, other.formulas);
}

#[test]
fn every_formula_parses_resolves_and_passes_the_axis_check() {
    let mut texts = String::new();
    for seed in 0..300 {
        let workspace = datagen::generate(&configuration(seed));
        let dimensions: Vec<DimensionDefinition> = workspace
            .dimensions
            .iter()
            .map(|dimension| dimension.definition().clone())
            .collect();
        let measures: Vec<MeasureDefinition> = workspace
            .inputs
            .iter()
            .map(|input| input.definition().clone())
            .chain(workspace.formulas.iter().map(|(result, _)| result.clone()))
            .collect();
        for (index, (result, text)) in workspace.formulas.iter().enumerate() {
            let parsed = formula::parse(text);
            assert_eq!(parsed.errors, [], "seed {seed}: {text}");
            let resolved = formula::resolve(&parsed.expression, &measures, &dimensions);
            assert_eq!(resolved.errors, [], "seed {seed}: {text}");
            let check = formula::check_axes(&resolved.expression, &result.axes);
            assert_eq!(check.errors, [], "seed {seed}: {text}");
            if index > 0 {
                let previous = format!("'計算{}'", index - 1);
                assert!(text.contains(&previous), "seed {seed}: {text}");
            }
            texts.push_str(text);
        }
    }
    for construct in [
        " + ", " - ", " * ", " / ", "-'", "SUM(", ", BY ", "'.'親'", "0",
    ] {
        assert!(texts.contains(construct), "no formula uses {construct}");
    }
}

#[test]
fn a_few_members_hold_most_cells() {
    let workspace = datagen::generate(&Configuration {
        seed: 1,
        dimension_count: 2,
        members_per_dimension: 50,
        input_count: 4,
        cells_per_input: 300,
        zipf_exponent: 1.2,
        chain_length: 0,
        maximum_depth: 0,
    });
    let input = workspace
        .inputs
        .iter()
        .find(|input| input.definition().axes.len() == 2)
        .expect("an input has both axes");
    let members = workspace
        .dimensions
        .iter()
        .find(|dimension| dimension.ulid() == input.definition().axes[0])
        .expect("the axis is a generated dimension")
        .len() as usize;
    let mut cells_per_member: HashMap<u32, usize> = HashMap::new();
    for member in input.base().column(0).as_primitive::<UInt32Type>().values() {
        *cells_per_member.entry(*member).or_default() += 1;
    }
    let mut counts: Vec<usize> = cells_per_member.into_values().collect();
    counts.sort_unstable_by(|left, right| right.cmp(left));
    let busiest: usize = counts.iter().take(members / 10).sum();
    let cells = input.base().num_rows();
    assert!(
        busiest * 10 > cells * 3,
        "the busiest tenth of {members} members holds {busiest} of {cells} cells"
    );
}
