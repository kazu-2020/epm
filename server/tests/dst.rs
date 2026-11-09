#[path = "../../storage/change_log/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use change_log::{Commit, Epoch};
use datagen::Workspace;
use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::{Dimension, MemberId};
use formula::CheckedExpression;
use futures_util::FutureExt;
use measure::{Measure, MeasureDefinition};
use object_store::ObjectStore;
use object_store::memory::InMemory;
use oracle::Oracle;
use server::{Edit, Engine, Exit, run};
use tokio::sync::{mpsc, watch};

const DEFAULT_SEEDS: u64 = 2000;

const TINY: Size = Size {
    products: 2,
    customers: 2,
    departments_per_kind: 1,
    quantity_cells: 50,
    account_fill: 0.02,
};

struct Model {
    workspace: Workspace,
    checked: Vec<(MeasureDefinition, CheckedExpression)>,
    engine: Engine,
}

impl Model {
    fn new() -> Model {
        let workspace = representative::generate(&TINY, 7);
        let engine = Engine::new(representative::generate(&TINY, 7)).expect("valid formulas");
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
        let checked = workspace
            .formulas
            .iter()
            .map(|formula| {
                let parsed = formula::parse(&formula.text);
                let resolved = formula::resolve(&parsed.expression, &definitions, &dimensions);
                let checked = formula::check_axes(&resolved.expression, &formula.result.axes)
                    .checked
                    .expect("the representative formulas are checked");
                (formula.result.clone(), checked)
            })
            .collect();
        Model {
            workspace,
            checked,
            engine,
        }
    }

    fn dimensions(&self) -> Vec<&Dimension> {
        self.workspace.dimensions.iter().collect()
    }

    fn fold(&self, log: &[Commit]) -> Vec<Measure> {
        let inputs = &self.workspace.inputs;
        let mut cells: Vec<BTreeMap<Vec<MemberId>, f64>> =
            inputs.iter().map(|input| input.cells().collect()).collect();
        for commit in log {
            let edit = Edit::decode(commit.payload.clone()).expect("the log holds cell edits");
            let input = inputs
                .iter()
                .position(|input| input.ulid() == edit.measure)
                .expect("an edit names an input measure");
            match edit.value {
                Some(value) => cells[input].insert(edit.coordinate, value),
                None => cells[input].remove(&edit.coordinate),
            };
        }
        inputs
            .iter()
            .zip(cells)
            .map(|(input, cells)| {
                let cells = cells.into_iter().collect();
                Measure::from_cells(input.definition().clone(), cells, &self.dimensions())
                    .expect("the folded cells fit the input measure")
            })
            .collect()
    }

    fn recalculate_all(&self, inputs: Vec<Measure>) -> Vec<Measure> {
        let dimensions = self.dimensions();
        let mut measures = inputs;
        for (result, expression) in &self.checked {
            let references: Vec<&Measure> = measures.iter().collect();
            let measure = column_engine::evaluate(expression, result, &references, &dimensions)
                .expect("the formula evaluates");
            measures.push(measure);
        }
        measures
    }
}

struct Owners {
    engine: Engine,
    inputs: Vec<MeasureDefinition>,
    seed: u64,
}

impl common::Server for Owners {
    fn start(&self, store: Arc<dyn ObjectStore>, workspace: String, epoch: u64) -> common::Started {
        let (requests, receiver) = mpsc::channel(64);
        let (recalculated, recalculated_receiver) = watch::channel(None);
        let engine = self.engine.clone();
        let exit = async move {
            match run(
                store,
                &workspace,
                Epoch(epoch),
                engine,
                receiver,
                recalculated,
            )
            .await
            {
                Exit::ChangeLog(exit) => Ok(exit),
                Exit::Engine(alarm) => Err(format!("{alarm:?}")),
            }
        };
        common::Started {
            exit: exit.boxed(),
            submit: common::submit_to(requests),
            passed_on: None,
            recalculated: Some(recalculated_receiver),
        }
    }

    fn payload(&self, request: &str) -> Bytes {
        let mut hasher = DefaultHasher::new();
        (self.seed, request).hash(&mut hasher);
        let hash = hasher.finish();
        let input = &self.inputs[(hash % self.inputs.len() as u64) as usize];
        let member = MemberId(u32::from(hash & 0x100 != 0));
        let value = (hash & 0x600 != 0).then(|| ((hash >> 12) % 8) as f64 / 4.0);
        Edit {
            measure: input.ulid,
            coordinate: vec![member; input.axes.len()],
            value,
        }
        .encode()
    }
}

fn simulate(model: &Model, seed: u64) -> common::Outcome {
    let owners = Owners {
        engine: model.engine.clone(),
        inputs: model
            .workspace
            .inputs
            .iter()
            .map(|input| input.definition().clone())
            .collect(),
        seed,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a runtime")
        .block_on(common::simulate(
            Arc::new(owners),
            Arc::new(InMemory::new()),
            "ws",
            seed,
        ))
}

fn cells(measure: &Measure) -> Vec<(Vec<u32>, String)> {
    exact(&measure.cells().collect::<Vec<_>>())
}

fn recalculated_log(model: &Model, log: &[Commit]) -> Result<Engine, String> {
    let mut engine = model.engine.clone();
    for commit in log {
        engine.apply(commit).map_err(|alarm| format!("{alarm:?}"))?;
    }
    Ok(engine)
}

fn final_values(model: &Model, outcome: &common::Outcome) -> common::Violations {
    let (_, log) = common::replay(&outcome.store);
    let expected = model.recalculate_all(model.fold(&log));
    let difference = match recalculated_log(model, &log) {
        Err(alarm) => Some(alarm),
        Ok(engine) => engine
            .measures()
            .iter()
            .zip(&expected)
            .find(|(measure, expected)| cells(measure) != cells(expected))
            .map(|(measure, _)| format!("{} differs", measure.definition().name)),
    };
    difference
        .map(|detail| ("finalValuesMatchEvaluate", detail))
        .into_iter()
        .collect()
}

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("DST_SEED") {
        return vec![seed.parse().expect("DST_SEED is a number")];
    }
    let count = std::env::var("DST_SEEDS").map_or(DEFAULT_SEEDS, |count| {
        count.parse().expect("DST_SEEDS is a number")
    });
    (0..count).collect()
}

#[test]
fn seeds_keep_the_invariants() {
    let model = Model::new();
    let seeds = seeds();
    let started = Instant::now();
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let results: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                let (seeds, model) = (&seeds, &model);
                scope.spawn(move || {
                    seeds
                        .iter()
                        .skip(thread)
                        .step_by(threads)
                        .map(|&seed| {
                            let outcome = simulate(model, seed);
                            let mut violations = common::check(&outcome);
                            violations.extend(final_values(model, &outcome));
                            let witnesses = common::witnesses(&outcome);
                            (seed, violations, witnesses, common::counts(&outcome))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a worker finishes"))
            .collect()
    });
    let mut violated: BTreeMap<&str, (usize, u64, String)> = BTreeMap::new();
    let mut witnessed: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut acknowledged, mut slots) = (0, 0);
    for (seed, violations, witnesses, counts) in results {
        acknowledged += counts.0;
        slots += counts.1;
        for (invariant, detail) in violations {
            let entry = violated.entry(invariant).or_insert((0, seed, detail));
            entry.0 += 1;
        }
        for witness in witnesses {
            *witnessed.entry(witness).or_default() += 1;
        }
    }
    println!("{} seeds in {:.1?}", seeds.len(), started.elapsed());
    println!("{acknowledged} requests acknowledged, {slots} slots written");
    for (witness, count) in &witnessed {
        println!("witnessed {witness} in {count} seeds");
    }
    for (invariant, (count, seed, detail)) in &violated {
        println!("violated {invariant} in {count} seeds, first DST_SEED={seed}: {detail}");
    }
    assert!(
        violated.is_empty(),
        "invariants broke: {:?}",
        violated.keys()
    );
    if seeds.len() >= 100 {
        let expected = [
            "commitAfterHandover",
            "crashAfterApplyBeforeReply",
            "crashBeforeRecalculated",
            "fenceFoundOlderCommits",
            "fenceMovedPastOldOwner",
            "oldOwnerStopped",
            "sameBytesResent",
            "storeRejectedPut",
            "twoAcknowledged",
        ];
        for witness in expected {
            assert!(witnessed.contains_key(witness), "no seed reached {witness}");
        }
    }
}

#[test]
fn a_seed_gives_the_same_history_every_time() {
    let model = Model::new();
    for seed in 0..30 {
        assert_eq!(
            simulate(&model, seed),
            simulate(&model, seed),
            "seed {seed}"
        );
    }
}

#[test]
fn final_values_agree_with_duckdb() {
    let model = Model::new();
    for seed in 0..20 {
        let outcome = simulate(&model, seed);
        let (_, log) = common::replay(&outcome.store);
        let inputs = model.fold(&log);
        let references: Vec<&Measure> = inputs.iter().collect();
        let mut oracle =
            Oracle::new(&model.dimensions(), &references).expect("DuckDB loads the inputs");
        let engine = recalculated_log(&model, &log).expect("the engine applies the log");
        let (engine_inputs, results) = engine.measures().split_at(inputs.len());
        for (input, expected) in engine_inputs.iter().zip(&inputs) {
            assert_eq!(cells(input), cells(expected), "seed {seed}");
        }
        for ((result, expression), measure) in model.checked.iter().zip(results) {
            let expected = oracle
                .evaluate(expression, result.ulid)
                .expect("DuckDB evaluates the formula");
            assert_eq!(
                cells(measure),
                exact(&expected),
                "seed {seed}: {}",
                result.name
            );
        }
    }
}
