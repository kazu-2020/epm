use std::path::PathBuf;
use std::time::Instant;

use datagen::representative::{self, Size};
use dimension::Dimension;
use measure::MeasureDefinition;

fn main() -> std::io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let usage = "usage: recalculate <directory> <column|evaluator>";
    let directory = PathBuf::from(arguments.next().expect(usage));
    let evaluate = match arguments.next().as_deref() {
        Some("column") => column_engine::evaluate,
        Some("evaluator") => evaluator::evaluate,
        _ => panic!("{usage}"),
    };
    let dimensions = representative::dimensions(&Size::FULL, bench::SEED);
    let dimensions: Vec<&Dimension> = dimensions.iter().collect();
    let mut measures = bench::read_measures(&directory, &dimensions)?;
    let formulas = representative::formulas();
    let definitions: Vec<MeasureDefinition> = measures
        .iter()
        .map(|measure| measure.definition().clone())
        .chain(formulas.iter().map(|formula| formula.result.clone()))
        .collect();
    let mut cells: usize = measures
        .iter()
        .map(|measure| measure.base().num_rows())
        .sum();
    let started = Instant::now();
    for formula in &formulas {
        let parsed = formula::parse(&formula.text);
        let resolved = formula::resolve(&parsed.expression, &definitions, &dimensions);
        let checked = formula::check_axes(&resolved.expression, &formula.result.axes)
            .checked
            .expect("the representative formulas pass the check");
        let step = Instant::now();
        let result = evaluate(
            &checked,
            &formula.result,
            &measures.iter().collect::<Vec<_>>(),
            &dimensions,
        )
        .expect("the representative formulas evaluate");
        eprintln!(
            "{}\t{}\t{:.3}s",
            formula.result.name,
            result.base().num_rows(),
            step.elapsed().as_secs_f64()
        );
        cells += result.base().num_rows();
        measures.push(result);
    }
    println!(
        "cells {cells}, recalculated in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
