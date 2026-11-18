#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the bench reports its measurements on the terminal"
)]

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use datagen::representative::{self, Size};
use formula::CheckedExpression;
use measure::{Measure, MeasureDefinition, MeasureGlobalId};

fn main() -> std::io::Result<()> {
    let usage = "usage: edit <directory> <edits per input>";
    let directory = PathBuf::from(std::env::args().nth(1).expect(usage));
    let edits: NonZeroUsize = std::env::args()
        .nth(2)
        .and_then(|edits| edits.parse().ok())
        .expect(usage);
    let dimensions = representative::dimensions(&Size::FULL, bench::SEED);
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
            let checked =
                workspace::compile(&formula.text, &formula.result, &definitions, &dimensions)
                    .expect("the representative formulas pass the check");
            (formula.result.global_id(), checked)
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
        .map(|(global_id, expression)| (*global_id, expression))
        .collect();
    let mut all = Vec::new();
    for input in 0..inputs {
        let name = measures[input].definition().name().to_string();
        let rows = measures[input].base_values().len();
        let mut timings = Vec::new();
        for edit in 0..edits.get() {
            let row = (edit * 7_919 * 104_729) % rows;
            let measure = &measures[input];
            // Not DeltaBuilder::set: it takes the coordinate in the declared order, and the base row gives it packed.
            let delta = measure
                .delta_from_packed(
                    vec![measure.base_packed_coordinates()[row]],
                    vec![Some(measure.base_values()[row] * 1.01 + 1.0)],
                )
                .expect("a packed coordinate of the base fits the measure");
            let global_id = measures[input].global_id();
            let started = Instant::now();
            let edited = column_engine::edit(&measures, [(global_id, delta)], &order, &dimensions)
                .expect("the edit recalculates");
            let evaluated = started.elapsed();
            let folding = Instant::now();
            measures = edited.iter().map(Measure::fold_deltas_or_compact).collect();
            let folded = folding.elapsed();
            let elapsed = evaluated + folded;
            let compacted = edited
                .iter()
                .zip(&measures)
                .filter(|(before, after)| before.delta_len() > 0 && after.delta_len() == 0)
                .count();
            timings.push(EditTiming { elapsed, compacted });
            let delta_len: usize = measures.iter().map(Measure::delta_len).sum();
            eprintln!(
                "{name}\t{edit}\t{:.3}s\tevaluate {:.3}s\tfold {:.3}s\tcompacted {compacted}\tdelta {delta_len}",
                elapsed.as_secs_f64(),
                evaluated.as_secs_f64(),
                folded.as_secs_f64()
            );
        }
        report(&name, &mut timings);
        all.extend(timings);
    }
    report("all", &mut all);
    Ok(())
}

struct EditTiming {
    elapsed: Duration,
    compacted: usize,
}

fn report(name: &str, timings: &mut [EditTiming]) {
    timings.sort_by_key(|timing| timing.elapsed);
    let rank = |percent: usize| (timings.len() * percent).div_ceil(100).saturating_sub(1);
    let at = |percent: usize| timings[rank(percent)].elapsed.as_secs_f64();
    let compacting =
        |timings: &[EditTiming]| timings.iter().filter(|timing| timing.compacted > 0).count();
    let slowest = &timings[rank(99)..];
    println!(
        "{name}\tedits {}\tp50 {:.3}s\tp99 {:.3}s\tmax {:.3}s\tcompacting {}\tcompacting at p99 and above {}/{}",
        timings.len(),
        at(50),
        at(99),
        at(100),
        compacting(timings),
        compacting(slowest),
        slowest.len()
    );
}
