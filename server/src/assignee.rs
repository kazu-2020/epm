use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use change_log::{
    Admit, AssigneeId, Commit, Request, RequestId, Sequence, Slot, Start, Submission,
};
use dimension::MemberIndex;
use measure::{CoordinateError, MeasureGlobalId};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode};
use tokio::sync::{mpsc, oneshot, watch};
use workspace::EditCheck;

use crate::apply::{Applier, Effect, Event};
use crate::edit;
use crate::snapshot::{self, Snapshot};
use crate::{Edit, Engine, EngineAlarm, Exit, Rejection};

const WAITING_REQUESTS: usize = 64;

pub struct Settings {
    pub busy_after: u64,
    pub calculation: Calculation,
    pub snapshot_every: u64,
}

pub enum Calculation {
    Threads(NonZeroUsize),
    Simulated(Duration),
}

impl Default for Settings {
    fn default() -> Settings {
        let threads = std::thread::available_parallelism()
            .ok()
            .and_then(|cores| NonZeroUsize::new(cores.get().saturating_sub(2)))
            .unwrap_or(NonZeroUsize::MIN);
        Settings {
            // shortcut: 1000 commits is a guess, set it from a benchmark of the recalculation time.
            busy_after: 1000,
            calculation: Calculation::Threads(threads),
            // shortcut: 256 slots is a guess, set it from the start-up time of a large workspace.
            snapshot_every: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lag {
    pub committed: Option<Sequence>,
    pub recalculated: Option<Sequence>,
}

#[derive(Debug, PartialEq)]
pub struct Stopped;

#[derive(Debug, PartialEq)]
pub struct Loading;

#[derive(Debug, PartialEq)]
pub enum SubmitError {
    Loading,
    Rejected(Rejection),
    Stopped,
}

#[derive(Debug, PartialEq)]
pub enum ReadError {
    Loading,
    NotCommitted(Lag),
    NoMeasure(MeasureGlobalId),
    Coordinate(CoordinateError),
    Stopped,
}

#[derive(Clone)]
pub struct Handle {
    submission_sender: mpsc::Sender<Submission<Rejection>>,
    engine_receiver: watch::Receiver<Arc<Engine>>,
    committed_count: CommittedCount,
    #[cfg(feature = "apply_before_commit")]
    commit_sender: mpsc::UnboundedSender<(Slot, Commit)>,
}

struct Admission {
    edit_check: EditCheck,
    engine_receiver: watch::Receiver<Arc<Engine>>,
    accepted_count: u64,
    busy_after: u64,
    committed_count: CommittedCount,
}

// Not the count of the commits read so far: while the assignee reads the log, the commits after them are committed too.
type CommittedCount = watch::Sender<Option<u64>>;

enum Calculator {
    Thread(rayon::ThreadPool),
    Simulated(Duration),
}

type Calculating = Pin<Box<dyn Future<Output = Result<Engine, EngineAlarm>> + Send>>;

type Writing = Pin<Box<dyn Future<Output = Option<Slot>> + Send>>;

pub fn start(
    engine: Engine,
    start: Start,
    settings: Settings,
    store: Arc<dyn ObjectStore>,
    workspace: String,
    id: AssigneeId,
) -> (Handle, impl Future<Output = Exit>) {
    assert!(
        engine.next.index() == start.request_ids.len() && engine.slot == start.slot,
        "the engine starts where the change log starts"
    );
    let (submission_sender, submission_receiver) = mpsc::channel(WAITING_REQUESTS);
    let (commit_sender, commit_receiver) = mpsc::unbounded_channel();
    let engine_next = engine.next.0;
    let committed_count = CommittedCount::new(None);
    let edit_check = engine.edit_check().clone();
    let engine = Arc::new(engine);
    let (engine_sender, engine_receiver) = watch::channel(Arc::clone(&engine));
    let admission = Admission {
        edit_check,
        engine_receiver: engine_receiver.clone(),
        accepted_count: engine_next,
        busy_after: settings.busy_after,
        committed_count: committed_count.clone(),
    };
    let handle = Handle {
        submission_sender,
        engine_receiver,
        committed_count: committed_count.clone(),
        #[cfg(feature = "apply_before_commit")]
        commit_sender: commit_sender.clone(),
    };
    let request_ids = start.request_ids.clone();
    let run = async move {
        let calculator = match settings.calculation {
            Calculation::Threads(threads) => Calculator::Thread(pool(threads)),
            Calculation::Simulated(cost) => Calculator::Simulated(cost),
        };
        // Not two spawned tasks: a crash aborts this one future, so the engine stops with the assignee.
        tokio::select! {
            biased;
            exit = change_log::serve(Arc::clone(&store), &workspace, id, start, admission, submission_receiver, commit_sender) => {
                Exit::ChangeLog(exit)
            }
            alarm = apply(
                Applier::new(engine, request_ids, settings.snapshot_every),
                commit_receiver,
                &calculator,
                &engine_sender,
                &committed_count,
                (&store, &workspace),
            ) => {
                Exit::Engine(alarm)
            }
        }
    };
    (handle, run)
}

async fn apply(
    mut applier: Applier,
    mut commit_receiver: mpsc::UnboundedReceiver<(Slot, Commit)>,
    calculator: &Calculator,
    engine_sender: &watch::Sender<Arc<Engine>>,
    committed_count: &CommittedCount,
    (store, workspace): (&Arc<dyn ObjectStore>, &str),
) -> EngineAlarm {
    let mut calculation = None;
    let mut writing: Option<Writing> = None;
    loop {
        let event = tokio::select! {
            biased;
            Some(commit) = commit_receiver.recv() => {
                // Not one commit at a time: a new assignee receives the whole log at once, and a batch of only its first commit would add one recalculation.
                let mut batch = vec![commit];
                while let Ok(commit) = commit_receiver.try_recv() {
                    batch.push(commit);
                }
                let last = batch.last().expect("the batch starts with a commit").1.sequence;
                raise(committed_count, last.0 + 1);
                if cfg!(feature = "skip_commit") {
                    batch.retain(|(_, commit)| commit.sequence != Sequence(1));
                }
                if cfg!(feature = "swap_commits") && batch.len() > 1 {
                    batch.swap(0, 1);
                }
                if cfg!(feature = "apply_twice") {
                    batch.extend(batch.clone());
                }
                Event::Committed(batch)
            }
            result = async { calculation.as_mut().expect("a calculation runs").await }, if calculation.is_some() => {
                calculation = None;
                Event::Finished(result)
            }
            written = async { writing.as_mut().expect("a snapshot write runs").await }, if writing.is_some() => {
                writing = None;
                Event::SnapshotWritten(written)
            }
            else => std::future::pending().await,
        };
        for effect in applier.step(event) {
            match effect {
                Effect::Calculate { engine, commits } => {
                    calculation = Some(calculator.calculate(engine, commits));
                }
                Effect::Publish(engine) => {
                    engine_sender.send_replace(engine);
                }
                Effect::WriteSnapshot(snapshot) => {
                    writing = Some(calculator.write(Arc::clone(store), workspace, snapshot));
                }
                Effect::Alarm(alarm) => return alarm,
            }
        }
    }
}

fn pool(threads: NonZeroUsize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.get())
        // Not the default handler: it aborts the process, and a panic in one batch must stop only this assignee.
        .panic_handler(drop::<Box<dyn std::any::Any + Send>>)
        .build()
        .expect("the engine pool starts")
}

fn on_pool(
    pool: &rayon::ThreadPool,
    sequences: RangeInclusive<Sequence>,
    batch: impl FnOnce() -> Result<Engine, EngineAlarm> + Send + 'static,
) -> Calculating {
    let (finished, result) = oneshot::channel();
    pool.spawn(move || {
        // Not an error: the assignee stopped and does not read the result.
        let _ = finished.send(batch());
    });
    Box::pin(async move {
        result
            .await
            .unwrap_or(Err(EngineAlarm::Panicked { sequences }))
    })
}

impl Calculator {
    fn calculate(&self, engine: Arc<Engine>, commits: Vec<(Slot, Commit)>) -> Calculating {
        match self {
            Calculator::Thread(pool) => {
                let first = engine.next;
                let last = commits.last().map_or(first, |(_, commit)| commit.sequence);
                on_pool(pool, first..=last, move || engine.apply_batch(&commits))
            }
            Calculator::Simulated(cost) => {
                let cost = *cost;
                Box::pin(async move {
                    tokio::time::sleep(cost).await;
                    engine.apply_batch(&commits)
                })
            }
        }
    }

    fn write(&self, store: Arc<dyn ObjectStore>, workspace: &str, snapshot: Snapshot) -> Writing {
        let count = snapshot.start.request_ids.len();
        let folder = format!("{workspace}/snapshot/{count:020}");
        let slot = snapshot.start.slot;
        let encoded = match self {
            Calculator::Thread(pool) => {
                let (finished, encoded) = oneshot::channel();
                pool.spawn(move || {
                    // Not an error: the assignee stopped and does not write the snapshot.
                    let _ = finished.send(snapshot::encode(&snapshot));
                });
                encoded
            }
            Calculator::Simulated(_) => {
                let (finished, encoded) = oneshot::channel();
                let _ = finished.send(snapshot::encode(&snapshot));
                encoded
            }
        };
        Box::pin(async move {
            let (objects, manifest) = encoded.await.ok()?;
            // shortcut: one PUT at a time, put the chunks in parallel when a snapshot of a large workspace takes too long.
            for object in objects {
                let path = Path::from(format!("{folder}/{}", object.path));
                store.put(&path, object.bytes.into()).await.ok()?;
            }
            let manifest_path = Path::from(format!("{folder}/manifest"));
            match store
                .put_opts(&manifest_path, manifest.into(), PutMode::Create.into())
                .await
            {
                // Not read back: a snapshot of the same count holds the same cells, because it is a cache of the same start of the log.
                Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Some(slot),
                Err(_) => None,
            }
        })
    }
}

impl Handle {
    pub async fn submit(
        &self,
        request_id: RequestId,
        edit: &Edit,
    ) -> Result<Sequence, SubmitError> {
        if self.committed_count().is_err() {
            return Err(if self.submission_sender.is_closed() {
                SubmitError::Stopped
            } else {
                SubmitError::Loading
            });
        }
        let payload = crate::edit::encode(edit);
        #[cfg(feature = "apply_before_commit")]
        let _ = self.commit_sender.send((
            self.engine_receiver.borrow().slot,
            Commit {
                request_id: request_id.clone(),
                sequence: self.engine_receiver.borrow().next,
                payload: payload.clone(),
            },
        ));
        let (submission, ack) = Submission::new(Request {
            request_id,
            payload,
        });
        self.submission_sender
            .send(submission)
            .await
            .map_err(|_| SubmitError::Stopped)?;
        let sequence = ack
            .await
            .map_err(|_| SubmitError::Stopped)?
            .map_err(SubmitError::Rejected)?;
        // Not left to the apply loop: this ACK can reach the caller before the loop receives the commit, and a read of this sequence would be refused.
        raise(&self.committed_count, sequence.0 + 1);
        Ok(sequence)
    }

    pub async fn read(
        &self,
        at_least: Option<Sequence>,
        cells: &[(MeasureGlobalId, Vec<MemberIndex>)],
    ) -> Result<(Option<Sequence>, Vec<Option<f64>>), ReadError> {
        let lag = self.lag().map_err(|Loading| {
            if self.submission_sender.is_closed() {
                ReadError::Stopped
            } else {
                ReadError::Loading
            }
        })?;
        // Not after the committed check: a read that can never succeed would get NotCommitted, and the caller would try it again.
        values(&self.engine_receiver.borrow(), cells)?;
        if lag.committed < at_least {
            return Err(ReadError::NotCommitted(lag));
        }
        let mut engine_receiver = self.engine_receiver.clone();
        let engine = Arc::clone(
            &*engine_receiver
                .wait_for(|engine| engine.recalculated() >= at_least)
                .await
                .map_err(|_| ReadError::Stopped)?,
        );
        Ok((engine.recalculated(), values(&engine, cells)?))
    }

    pub fn lag(&self) -> Result<Lag, Loading> {
        // Not the committed count first: a batch that finishes between the two reads would show more recalculated than committed.
        let recalculated = self.engine_receiver.borrow().recalculated();
        let committed_count = self.committed_count()?;
        Ok(Lag {
            committed: committed_count.checked_sub(1).map(Sequence),
            recalculated,
        })
    }

    pub fn definitions(&self) -> Result<Arc<Engine>, Loading> {
        self.committed_count()?;
        Ok(Arc::clone(&self.engine_receiver.borrow()))
    }

    fn committed_count(&self) -> Result<u64, Loading> {
        (*self.committed_count.borrow()).ok_or(Loading)
    }

    pub async fn wait_loaded(&self) -> Result<(), Stopped> {
        let mut committed_receiver = self.committed_count.subscribe();
        tokio::select! {
            _ = committed_receiver.wait_for(Option::is_some) => Ok(()),
            () = self.submission_sender.closed() => Err(Stopped),
        }
    }

    pub fn engine_receiver(&self) -> watch::Receiver<Arc<Engine>> {
        self.engine_receiver.clone()
    }
}

fn values(
    engine: &Engine,
    cells: &[(MeasureGlobalId, Vec<MemberIndex>)],
) -> Result<Vec<Option<f64>>, ReadError> {
    cells
        .iter()
        .map(|(measure, coordinate)| {
            engine
                .measures()
                .iter()
                .find(|candidate| candidate.global_id() == *measure)
                .ok_or(ReadError::NoMeasure(*measure))?
                .get(coordinate)
                .map_err(ReadError::Coordinate)
        })
        .collect()
}

impl Admit for Admission {
    type Rejection = Rejection;

    fn admit(&mut self, payload: &Bytes) -> Result<(), Rejection> {
        let behind = self
            .accepted_count
            .saturating_sub(self.engine_receiver.borrow().next.0);
        if behind > self.busy_after {
            return Err(Rejection::Busy { behind });
        }
        if !cfg!(feature = "no_admission_check") {
            edit::check(&self.edit_check, payload).map_err(Rejection::Invalid)?;
        }
        self.accepted_count += 1;
        Ok(())
    }

    fn replay(&mut self, _payload: &Bytes) {
        self.accepted_count += 1;
    }

    fn ready(&mut self, next: Sequence) {
        self.committed_count.send_replace(Some(next.0));
    }
}

fn raise(committed_count: &CommittedCount, count: u64) {
    committed_count.send_if_modified(|committed_count| match committed_count {
        Some(committed_count) if *committed_count < count => {
            *committed_count = count;
            true
        }
        _ => false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_batch_that_panics_is_an_alarm_with_its_sequences() {
        let sequences = Sequence(3)..=Sequence(4);
        let calculating = on_pool(&pool(NonZeroUsize::MIN), sequences.clone(), || {
            panic!("a bug in the calculation")
        });
        assert_eq!(
            calculating.await.err(),
            Some(EngineAlarm::Panicked { sequences })
        );
    }
}
