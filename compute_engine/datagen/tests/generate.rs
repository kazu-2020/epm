use std::collections::HashMap;

use arrow_array::RecordBatch;
use arrow_array::cast::AsArray;
use arrow_array::types::UInt32Type;
use datagen::Configuration;
use dimension::Dimension;
use measure::MeasureDefinition;
use workspace::UncheckedWorkspace;

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

fn batches(workspace: &UncheckedWorkspace) -> Vec<RecordBatch> {
    let dimensions = workspace
        .dimensions
        .iter()
        .map(|dimension| dimension.batch().clone());
    let inputs = workspace.inputs.iter().map(|input| input.to_record_batch());
    dimensions.chain(inputs).collect()
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
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let measures: Vec<MeasureDefinition> = workspace
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
        for (index, workspace::Formula { result, text }) in workspace.formulas.iter().enumerate() {
            let compiled = workspace::compile(text, result, &measures, &dimensions);
            assert_eq!(compiled.err(), None, "seed {seed}: {text}");
            if index > 0 {
                let previous = format!("'計算{}'", index - 1);
                assert!(text.contains(&previous), "seed {seed}: {text}");
            }
            texts.push_str(text);
        }
    }
    for construct in [
        " + ", " - ", " * ", " / ", "-'", "SUM(", ", BY ", "'.'親'", "0", "': 'm", "FILTER(",
        "' = 'm", " IN ('m", ", 'm",
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
    for member in input
        .to_record_batch()
        .column(0)
        .as_primitive::<UInt32Type>()
        .values()
    {
        *cells_per_member.entry(*member).or_default() += 1;
    }
    let mut counts: Vec<usize> = cells_per_member.into_values().collect();
    counts.sort_unstable_by(|left, right| right.cmp(left));
    let busiest: usize = counts.iter().take(members / 10).sum();
    let cells = input.to_record_batch().num_rows();
    assert!(
        busiest * 10 > cells * 3,
        "the busiest tenth of {members} members holds {busiest} of {cells} cells"
    );
}

#[test]
fn the_representative_model_declares_its_axes_in_packing_order() {
    let size = datagen::representative::Size {
        products: 4,
        customers: 3,
        departments_per_kind: 2,
        quantity_cells: 130,
        account_fill: 0.5,
    };
    let workspace = datagen::representative::generate(&size, 1);
    let packing_order = |axis| {
        workspace
            .dimensions
            .iter()
            .find(|dimension| dimension.ulid() == axis)
            .expect("every axis is a dimension")
            .packing_order()
    };
    let declared = workspace
        .inputs
        .iter()
        .map(|input| input.definition())
        .chain(workspace.formulas.iter().map(|formula| &formula.result));

    for definition in declared {
        assert!(
            definition
                .axes
                .is_sorted_by_key(|&axis| packing_order(axis)),
            "{}",
            definition.name
        );
    }
}

#[test]
fn the_quantity_has_exactly_the_requested_cells_when_they_do_not_divide_evenly() {
    let size = datagen::representative::Size {
        products: 4,
        customers: 3,
        departments_per_kind: 2,
        quantity_cells: 130,
        account_fill: 0.5,
    };
    let workspace = datagen::representative::generate(&size, 1);
    let quantity = workspace
        .inputs
        .iter()
        .find(|input| input.definition().name == "数量")
        .expect("the quantity is an input");
    assert_eq!(quantity.cells().len(), size.quantity_cells);
}
