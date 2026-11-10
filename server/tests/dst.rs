#[path = "../../storage/change_log/tests/common/mod.rs"]
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use change_log::{Commit, Epoch, Request, Sequence};
use common::Record;
use datagen::Workspace;
use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::{Dimension, MemberId};
use formula::CheckedExpression;
use futures_util::{FutureExt, StreamExt, stream};
use measure::{Measure, MeasureDefinition};
use object_store::ObjectStore;
use object_store::memory::InMemory;
use oracle::Oracle;
use server::{Calculation, Edit, Engine, Exit, Owner, Settings};

const DEFAULT_SEEDS: u64 = 2000;
const CALCULATION: Duration = Duration::from_millis(250);

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
        let settings = Settings {
            calculation: Calculation::Inline(CALCULATION),
            ..Settings::default()
        };
        let (owner, handle) = Owner::new(self.engine.clone(), settings);
        let recalculated = stream::unfold(handle.published(), |mut published| async move {
            published.changed().await.ok()?;
            let sequence = published.borrow_and_update().recalculated();
            Some((sequence, published))
        })
        .filter_map(std::future::ready);
        let submit: common::Submit = Arc::new(move |request: Request| {
            let handle = handle.clone();
            async move {
                let edit = Edit::decode(request.payload).expect("the simulation sends edits");
                handle.submit(request.request_id, &edit).await.ok()
            }
            .boxed()
        });
        let exit = async move {
            match owner.run(store, &workspace, Epoch(epoch)).await {
                Exit::ChangeLog(exit) => Ok(exit),
                Exit::Engine(alarm) => Err(format!("{alarm:?}")),
            }
        };
        common::Started {
            exit: exit.boxed(),
            submit,
            passed_on: None,
            recalculated: Some(recalculated.boxed()),
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

fn recalculated_log(model: &Model, log: &[Commit], batch: usize) -> Result<Engine, String> {
    let mut engine = model.engine.clone();
    for commits in log.chunks(batch) {
        engine = engine
            .apply_batch(commits)
            .map_err(|alarm| format!("{alarm:?}"))?;
    }
    Ok(engine)
}

fn final_values(model: &Model, seed: u64, outcome: &common::Outcome) -> common::Violations {
    let (_, log) = common::replay(&outcome.store);
    let expected = model.recalculate_all(model.fold(&log));
    let batch = 1 + (seed % 4) as usize;
    let difference = match recalculated_log(model, &log, batch) {
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

fn recalculation_witnesses(outcome: &common::Outcome) -> BTreeSet<&'static str> {
    let mut seen = BTreeSet::new();
    let (segments, _) = common::replay(&outcome.store);
    let written_by: BTreeMap<Sequence, u64> = segments
        .values()
        .flat_map(|segment| {
            segment
                .commits
                .iter()
                .map(|commit| (commit.sequence, segment.epoch.0))
        })
        .collect();
    let mut recalculates: BTreeSet<u64> = BTreeSet::new();
    let mut epochs: BTreeMap<usize, u64> = BTreeMap::new();
    let mut crashed: BTreeSet<u64> = BTreeSet::new();
    let mut recalculated_so_far: BTreeMap<u64, Sequence> = BTreeMap::new();
    let mut acked_so_far: BTreeMap<u64, BTreeSet<Sequence>> = BTreeMap::new();
    for record in &outcome.history {
        match record {
            Record::Granted {
                server,
                epoch,
                recalculates: granted,
                ..
            } => {
                epochs.insert(*server, *epoch);
                if *granted {
                    recalculates.insert(*epoch);
                }
            }
            Record::Crashed { server } => crashed.extend(epochs.get(server)),
            Record::Recalculated { epoch, sequence } => {
                // Not None before the first record: every owner recalculates from Commit Sequence 0, so its first batch starts there.
                let first = recalculated_so_far
                    .insert(*epoch, *sequence)
                    .map_or(Sequence(0), Sequence::next);
                let acked = acked_so_far.get(epoch).map_or(0, |acked| {
                    acked
                        .range(first..=*sequence)
                        .filter(|acked| written_by.get(acked) == Some(epoch))
                        .count()
                });
                if acked >= 2 {
                    seen.insert("batchHeldTwoAckedCommits");
                }
            }
            Record::Acked {
                epoch, sequence, ..
            } => {
                acked_so_far.entry(*epoch).or_default().insert(*sequence);
                if recalculates.contains(epoch) && recalculated_so_far.get(epoch) < Some(sequence) {
                    seen.insert("ackBeforeRecalculated");
                }
            }
            _ => {}
        }
    }
    let newest_acked = |epoch: &u64| acked_so_far.get(epoch).and_then(BTreeSet::last);
    if crashed.iter().any(|epoch| {
        recalculates.contains(epoch) && newest_acked(epoch) > recalculated_so_far.get(epoch)
    }) {
        seen.insert("crashBeforeRecalculated");
    }
    seen
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
                            violations.extend(final_values(model, seed, &outcome));
                            let mut witnesses = common::witnesses(&outcome);
                            witnesses.extend(recalculation_witnesses(&outcome));
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
            "ackBeforeRecalculated",
            "batchHeldTwoAckedCommits",
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
        let engine =
            recalculated_log(&model, &log, log.len().max(1)).expect("the engine applies the log");
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
