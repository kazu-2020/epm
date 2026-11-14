#[path = "../../storage/change_log/tests/common/mod.rs"]
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use change_log::{Commit, Epoch, Request, Sequence, Slot, Start};
use common::Record;
use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::{Dimension, MemberIndex};
use futures_util::{FutureExt, StreamExt, TryStreamExt, stream};
use measure::{Measure, MeasureDefinition};
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use oracle::Oracle;
use server::snapshot::Manifest;
use server::{Calculation, Edit, Engine, Exit, Rejection, Settings, SubmitError};
use tokio::sync::oneshot;
use workspace::Workspace;

const DEFAULT_SEEDS: u64 = 2000;
const CALCULATION: Duration = Duration::from_millis(250);

struct Model {
    workspace: Workspace,
    engine: Engine,
}

impl Model {
    fn new() -> Model {
        let workspace = Workspace::try_new(representative::generate(&Size::TINY, 7))
            .expect("a valid workspace");
        let engine = Engine::new(
            Workspace::try_new(representative::generate(&Size::TINY, 7))
                .expect("a valid workspace"),
        )
        .expect("valid formulas");
        Model { workspace, engine }
    }

    fn dimensions(&self) -> Vec<&Dimension> {
        self.workspace.dimensions().iter().collect()
    }

    fn fold(&self, log: &[Commit]) -> Option<Vec<Measure>> {
        let inputs = self.workspace.inputs();
        let mut cells: Vec<BTreeMap<Vec<MemberIndex>, f64>> =
            inputs.iter().map(|input| input.cells().collect()).collect();
        for commit in log {
            let edit =
                server::edit::decode(commit.payload.clone()).expect("the log holds cell edits");
            let input = inputs
                .iter()
                .position(|input| input.global_id() == edit.measure)?;
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
                Measure::from_cells(input.definition().clone(), cells, &self.dimensions()).ok()
            })
            .collect()
    }

    fn recalculate_all(&self, inputs: Vec<Measure>) -> Vec<Measure> {
        let dimensions = self.dimensions();
        let mut measures = inputs;
        for formula in self.workspace.formulas() {
            let references: Vec<&Measure> = measures.iter().collect();
            let (result, expression) = (formula.result(), formula.expression());
            let measure = column_engine::evaluate(expression, result, &references, &dimensions)
                .expect("the formula evaluates");
            measures.push(measure);
        }
        measures
    }
}

struct Assignees {
    inputs: Vec<MeasureDefinition>,
    results: Vec<MeasureDefinition>,
    seed: u64,
    restored: Arc<Mutex<Vec<(u64, Start)>>>,
}

#[derive(Debug, PartialEq)]
struct Run {
    outcome: common::Outcome,
    restored: Vec<(u64, Start)>,
    snapshots: BTreeMap<String, Bytes>,
}

impl common::Server for Assignees {
    fn start(&self, store: Arc<dyn ObjectStore>, workspace: String, epoch: u64) -> common::Started {
        let mut settings = Settings {
            calculation: Calculation::Simulated(CALCULATION),
            snapshot_every: 1 + self.seed % 3,
            ..Settings::default()
        };
        if self.seed.is_multiple_of(2) {
            settings.busy_after = self.seed / 2 % 3;
        }
        let (started, handle) = oneshot::channel();
        // Not a watch: it wakes its waiters in a random order, and the requests would reach the assignee in another order on each run.
        let handle = handle.map(Result::ok).shared();
        let restored = Arc::clone(&self.restored);
        let exit = async move {
            let definition = Workspace::try_new(representative::generate(&Size::TINY, 7))
                .expect("a valid workspace");
            let (engine, start) = server::restore(store.as_ref(), &workspace, definition)
                .await
                .map_err(|error| format!("{error:?}"))?;
            restored
                .lock()
                .expect("restored")
                .push((epoch, start.clone()));
            let (handle, assignee) =
                server::start(engine, start, settings, store, workspace, Epoch(epoch));
            let _ = started.send(handle);
            match assignee.await {
                Exit::ChangeLog(exit) => Ok(exit),
                Exit::Engine(alarm) => Err(format!("{alarm:?}")),
            }
        };
        let recalculated = stream::once(handle.clone())
            .filter_map(std::future::ready)
            .flat_map(|handle| {
                // Not only the changes: an assignee from a snapshot starts with its commits recalculated.
                let first = handle.engine_receiver().borrow().recalculated();
                stream::once(std::future::ready(first)).chain(stream::unfold(
                    handle.engine_receiver(),
                    |mut engine_receiver| async move {
                        engine_receiver.changed().await.ok()?;
                        let sequence = engine_receiver.borrow_and_update().recalculated();
                        Some((sequence, engine_receiver))
                    },
                ))
            })
            .filter_map(std::future::ready);
        let submit: common::Submit = Arc::new(move |request: Request| {
            let handle = handle.clone();
            async move {
                let handle = handle.await?;
                let edit =
                    server::edit::decode(request.payload).expect("the simulation sends edits");
                match handle.submit(request.request_id, &edit).await {
                    Ok(sequence) => Some(Ok(sequence)),
                    Err(SubmitError::Rejected(Rejection::Busy { .. })) => {
                        Some(Err(common::Refusal::Busy))
                    }
                    Err(SubmitError::Rejected(Rejection::Invalid(_))) => {
                        Some(Err(common::Refusal::Invalid))
                    }
                    Err(SubmitError::Stopped) => None,
                }
            }
            .boxed()
        });
        common::Started {
            exit: exit.boxed(),
            submit,
            commit_receiver: None,
            recalculated: Some(recalculated.boxed()),
        }
    }

    fn payload(&self, request: &str) -> Bytes {
        let mut hasher = DefaultHasher::new();
        (self.seed, request).hash(&mut hasher);
        let hash = hasher.finish();
        let input = &self.inputs[(hash % self.inputs.len() as u64) as usize];
        let result = &self.results[(hash % self.results.len() as u64) as usize];
        let member = MemberIndex(u32::from(hash & 0x100 != 0));
        let value = (hash & 0x600 != 0).then(|| ((hash >> 12) % 8) as f64 / 4.0);
        let (measure, axes) = match (hash >> 20) % 8 {
            0 => (result, result.axes.len()),
            1 => (input, input.axes.len() + 1),
            _ => (input, input.axes.len()),
        };
        server::edit::encode(&Edit {
            measure: measure.global_id,
            coordinate: vec![member; axes],
            value,
        })
    }
}

fn simulate(model: &Model, seed: u64) -> Run {
    let restored = Arc::new(Mutex::new(Vec::new()));
    let assignees = Assignees {
        inputs: model
            .workspace
            .inputs()
            .iter()
            .map(|input| input.definition().clone())
            .collect(),
        results: model
            .workspace
            .formulas()
            .iter()
            .map(|formula| formula.result().clone())
            .collect(),
        seed,
        restored: Arc::clone(&restored),
    };
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a runtime")
        .block_on(async {
            let outcome =
                common::simulate(Arc::new(assignees), Arc::clone(&inner), "ws", seed).await;
            let listed: Vec<_> = inner
                .list(Some(&Path::from("ws/snapshot")))
                .try_collect()
                .await
                .expect("list the snapshots");
            let mut snapshots = BTreeMap::new();
            for meta in listed {
                let body = inner
                    .get(&meta.location)
                    .await
                    .expect("get a snapshot object")
                    .bytes()
                    .await
                    .expect("read a snapshot object");
                snapshots.insert(meta.location.to_string(), body);
            }
            let restored = std::mem::take(&mut *restored.lock().expect("restored"));
            Run {
                outcome,
                restored,
                snapshots,
            }
        })
}

fn cells(measure: &Measure) -> Vec<(Vec<u32>, String)> {
    exact(&measure.cells().collect::<Vec<_>>())
}

fn recalculated_log(model: &Model, log: &[Commit], batch: usize) -> Result<Engine, String> {
    let mut engine = model.engine.clone();
    let log: Vec<(Slot, Commit)> = log
        .iter()
        .map(|commit| (Slot::default(), commit.clone()))
        .collect();
    for commits in log.chunks(batch) {
        engine = engine
            .apply_batch(commits)
            .map_err(|alarm| format!("{alarm:?}"))?;
    }
    Ok(engine)
}

fn final_values(model: &Model, seed: u64, outcome: &common::Outcome) -> common::Violations {
    let (_, log) = common::replay(&outcome.store);
    let Some(inputs) = model.fold(&log) else {
        return [(
            "finalValuesMatchEvaluate",
            "the log holds an edit that does not fit an input measure".to_owned(),
        )]
        .into();
    };
    let expected = model.recalculate_all(inputs);
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
                // Not None before the first record: every assignee recalculates from Commit Sequence 0, so its first batch starts there.
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

fn snapshots_match_log(model: &Model, run: &Run) -> common::Violations {
    let (segments, log) = common::replay(&run.outcome.store);
    let definitions: Vec<MeasureDefinition> = model
        .workspace
        .inputs()
        .iter()
        .map(|input| input.definition().clone())
        .collect();
    let mut violations = common::Violations::new();
    for (key, manifest) in &run.snapshots {
        let Some(folder) = key.strip_suffix("/manifest") else {
            continue;
        };
        let mut violate = |detail: String| {
            violations
                .entry("snapshotMatchesLog")
                .or_insert(format!("{folder}: {detail}"));
        };
        let manifest = Manifest::decode(manifest.clone()).expect("the server writes manifests");
        let objects: Option<Vec<Bytes>> = manifest
            .paths()
            .iter()
            .map(|path| run.snapshots.get(&format!("{folder}/{path}")).cloned())
            .collect();
        let Some(objects) = objects else {
            violate("an object of the manifest is missing".to_owned());
            continue;
        };
        let snapshot = match manifest.decode_objects(objects, &definitions, &model.dimensions()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                violate(format!("{error:?}"));
                continue;
            }
        };
        let start = &snapshot.start;
        let count = start.request_ids.len();
        let Some(prefix) = log.get(..count) else {
            violate(format!("{count} commits, but the log holds {}", log.len()));
            continue;
        };
        if prefix
            .iter()
            .map(|commit| &commit.request_id)
            .ne(&start.request_ids)
        {
            violate("the Request IDs are not the start of the log".to_owned());
        }
        let last = count.checked_sub(1).map(|last| Sequence(last as u64));
        let in_slot = segments.get(&start.slot).is_some_and(|segment| {
            segment
                .commits
                .iter()
                .any(|commit| Some(commit.sequence) == last)
        });
        if last.is_some() && !in_slot {
            violate(format!("{:?} does not hold {last:?}", start.slot));
        }
        let expected = model.fold(prefix).expect("the log holds valid edits");
        if snapshot
            .inputs
            .iter()
            .zip(&expected)
            .any(|(input, expected)| cells(input) != cells(expected))
        {
            violate("the cells differ from the start of the log".to_owned());
        }
    }
    violations
}

fn snapshot_witnesses(run: &Run) -> BTreeSet<&'static str> {
    let counts: BTreeMap<u64, u64> = run
        .restored
        .iter()
        .filter(|(_, start)| !start.request_ids.is_empty())
        .map(|(epoch, start)| (*epoch, start.request_ids.len() as u64))
        .collect();
    let mut seen = BTreeSet::new();
    if !counts.is_empty() {
        seen.insert("startedFromSnapshot");
    }
    let acked_from_snapshot = run.outcome.history.iter().any(|record| {
        matches!(record, Record::Acked { epoch, sequence, .. }
            if counts.get(epoch).is_some_and(|count| sequence.0 < *count))
    });
    if acked_from_snapshot {
        seen.insert("resendAckedFromSnapshot");
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
                            let run = simulate(model, seed);
                            let outcome = &run.outcome;
                            let mut violations = common::check(outcome);
                            violations.extend(final_values(model, seed, outcome));
                            violations.extend(snapshots_match_log(model, &run));
                            let mut witnesses = common::witnesses(outcome);
                            witnesses.extend(recalculation_witnesses(outcome));
                            witnesses.extend(snapshot_witnesses(&run));
                            (seed, violations, witnesses, common::counts(outcome))
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
            "fenceMovedPastOldAssignee",
            "oldAssigneeStopped",
            "refusedBusy",
            "refusedInvalid",
            "sameBytesResent",
            "startedFromSnapshot",
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
        let outcome = simulate(&model, seed).outcome;
        let (_, log) = common::replay(&outcome.store);
        let inputs = model.fold(&log).expect("the log holds valid edits");
        let references: Vec<&Measure> = inputs.iter().collect();
        let mut oracle =
            Oracle::new(&model.dimensions(), &references).expect("DuckDB loads the inputs");
        let engine =
            recalculated_log(&model, &log, log.len().max(1)).expect("the engine applies the log");
        let (engine_inputs, results) = engine.measures().split_at(inputs.len());
        for (input, expected) in engine_inputs.iter().zip(&inputs) {
            assert_eq!(cells(input), cells(expected), "seed {seed}");
        }
        for (formula, measure) in model.workspace.formulas().iter().zip(results) {
            let result = formula.result();
            let expected = oracle
                .evaluate(formula.expression(), result.global_id)
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
