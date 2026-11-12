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
        .map(|measure| measure.base_values().len())
        .sum();
    for measure in &measures {
        eprintln!(
            "{}\t{}\tinput\t{} axes\t{} bytes",
            measure.definition().name,
            measure.base_values().len(),
            measure.definition().axes.len(),
            measure.base_packed_coordinates().inner().capacity()
                + measure.base_values().inner().capacity()
        );
    }
    eprintln!("after read\t{}", memory());
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
            "{}\t{}\t{:.3}s\t{} axes\t{} bytes\t{}",
            formula.result.name,
            result.base_values().len(),
            step.elapsed().as_secs_f64(),
            result.definition().axes.len(),
            result.base_packed_coordinates().inner().capacity()
                + result.base_values().inner().capacity(),
            memory()
        );
        cells += result.base_values().len();
        measures.push(result);
    }
    println!(
        "cells {cells}, recalculated in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    // Not always: one thread hashes every cell in about 0.35 s, and hyperfine would time it.
    let hash = std::env::var_os("RECALCULATE_HASH").is_some();
    let mut seen = std::collections::HashMap::new();
    for measure in &measures {
        let mut fresh = |buffer: &arrow_buffer::Buffer| {
            if seen
                .insert(buffer.data_ptr().as_ptr() as usize, buffer.capacity())
                .is_none()
            {
                buffer.capacity()
            } else {
                0
            }
        };
        let keys = fresh(measure.base_packed_coordinates().inner());
        let values = fresh(measure.base_values().inner());
        let hash = if hash {
            let mut hasher = std::hash::DefaultHasher::new();
            std::hash::Hash::hash_slice(measure.base_packed_coordinates(), &mut hasher);
            for value in measure.base_values().iter() {
                std::hash::Hash::hash(&value.to_bits(), &mut hasher);
            }
            format!("\thash {:016x}", std::hash::Hasher::finish(&hasher))
        } else {
            String::new()
        };
        eprintln!(
            "unique\t{}\t{}\tkeys {keys}\tvalues {values}{hash}",
            measure.definition().name,
            measure.base_values().len(),
        );
    }
    Ok(())
}

fn memory() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("VmRSS") || line.starts_with("VmHWM"))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(""))
        .chain(
            std::fs::read_to_string("/proc/self/stat")
                .unwrap_or_default()
                .rsplit(") ")
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .enumerate()
                .filter_map(|(index, field)| {
                    let label = match index {
                        7 => "minflt",
                        9 => "majflt",
                        11 => "utime",
                        12 => "stime",
                        _ => return None,
                    };
                    Some(format!("{label}:{field}"))
                }),
        )
        .collect::<Vec<_>>()
        .join(" ")
}
