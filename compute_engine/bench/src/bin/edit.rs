use std::path::PathBuf;
use std::time::{Duration, Instant};

use datagen::representative::{self, Size};
use dimension::{Dimension, MemberIndex};
use formula::CheckedExpression;
use measure::{MeasureDefinition, MeasureGlobalId};

fn main() -> std::io::Result<()> {
    let usage = "usage: edit <directory> <edits per input>";
    let directory = PathBuf::from(std::env::args().nth(1).expect(usage));
    let edits: usize = std::env::args()
        .nth(2)
        .and_then(|edits| edits.parse().ok())
        .expect(usage);
    let dimensions = representative::dimensions(&Size::FULL, bench::SEED);
    let dimensions: Vec<&Dimension> = dimensions.iter().collect();
    let mut measures = bench::read_measures(&directory, &dimensions)?;
    let inputs = measures.len();
    let formulas = representative::formulas();
    let definitions: Vec<MeasureDefinition> = measures
        .iter()
        .map(|measure| measure.definition().clone())
        .chain(formulas.iter().map(|formula| formula.result.clone()))
        .collect();
    let checked: Vec<(MeasureGlobalId, CheckedExpression)> = formulas
        .iter()
        .map(|formula| {
            let parsed = formula::parse(&formula.text);
            let resolved = formula::resolve(&parsed.expression, &definitions, &dimensions);
            let checked = formula::check_axes(&resolved.expression, &formula.result.axes)
                .checked
                .expect("the representative formulas pass the check");
            (formula.result.ulid, checked)
        })
        .collect();
    for (formula, (_, expression)) in formulas.iter().zip(&checked) {
        let references: Vec<_> = measures.iter().collect();
        let result = column_engine::evaluate(expression, &formula.result, &references, &dimensions)
            .expect("the representative formulas evaluate");
        measures.push(result);
    }
    let order: Vec<(MeasureGlobalId, &CheckedExpression)> = checked
        .iter()
        .map(|(ulid, expression)| (*ulid, expression))
        .collect();
    let mut all = Vec::new();
    for input in 0..inputs {
        let name = measures[input].definition().name.clone();
        let rows = measures[input].values().len();
        let mut times = Vec::new();
        for edit in 0..edits {
            let row = (edit * 7_919 * 104_729) % rows;
            let measure = &measures[input];
            let coordinate: Vec<MemberIndex> = measure
                .layout()
                .members(measure.keys()[row])
                .map(MemberIndex)
                .collect();
            let value = measure.values()[row];
            let mut delta = measures[input].delta();
            delta
                .set(&coordinate, value * 1.01 + 1.0)
                .expect("the coordinate fits");
            let ulid = measures[input].ulid();
            let started = Instant::now();
            measures = column_engine::edit(&measures, [(ulid, delta)], &order, &dimensions)
                .expect("the edit recalculates");
            times.push(started.elapsed());
            eprintln!("{name}\t{edit}\t{:.3}s", started.elapsed().as_secs_f64());
            // Not timed: each edit starts from a compacted base, so the times leave out the merge of the deltas that the server keeps.
            for measure in &mut measures {
                if measure.delta_len() > 0 {
                    *measure = measure.compact();
                }
            }
        }
        report(&name, &mut times);
        all.extend(times);
    }
    report("all", &mut all);
    Ok(())
}

fn report(name: &str, times: &mut [Duration]) {
    times.sort();
    let at = |quantile: f64| {
        times[((times.len() as f64 * quantile).ceil() as usize).saturating_sub(1)].as_secs_f64()
    };
    println!(
        "{name}\tedits {}\tp50 {:.3}s\tp99 {:.3}s\tmax {:.3}s",
        times.len(),
        at(0.5),
        at(0.99),
        at(1.0)
    );
}
