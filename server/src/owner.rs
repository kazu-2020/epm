use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use change_log::{Commit, Epoch, Request, RequestId, Sequence, Submission};
use dimension::MemberId;
use measure::{MeasureError, MeasureUlid};
use object_store::ObjectStore;
use tokio::sync::{mpsc, oneshot, watch};

use crate::apply::{Applier, Effect, Event};
use crate::{Edit, Engine, EngineAlarm, Exit, Rejection};

const WAITING_REQUESTS: usize = 64;

pub struct Settings {
    pub busy_after: u64,
    pub calculation: Calculation,
}

pub enum Calculation {
    Threads(NonZeroUsize),
    // Not for production: the calculation blocks the task that also writes the change log. The simulation uses it to stay reproducible.
    Inline(Duration),
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lag {
    pub committed: Option<Sequence>,
    pub recalculated: Option<Sequence>,
}

impl Lag {
    fn behind(&self) -> u64 {
        let count = |sequence: Option<Sequence>| sequence.map_or(0, |sequence| sequence.0 + 1);
        count(self.committed).saturating_sub(count(self.recalculated))
    }
}

#[derive(Debug, PartialEq)]
pub enum SubmitError {
    Rejected(Rejection),
    Stopped,
}

#[derive(Debug, PartialEq)]
pub enum ReadError {
    NotCommitted(Lag),
    NoMeasure(MeasureUlid),
    Coordinate(MeasureError),
    Stopped,
}

pub struct Owner {
    calculation: Calculation,
    requests: mpsc::Receiver<Submission<Rejection>>,
    passed_on: mpsc::UnboundedSender<Commit>,
    commits: mpsc::UnboundedReceiver<Commit>,
    published: watch::Sender<Arc<Engine>>,
    committed_count: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct Handle {
    requests: mpsc::Sender<Submission<Rejection>>,
    published: watch::Receiver<Arc<Engine>>,
    committed_count: Arc<AtomicU64>,
    busy_after: u64,
    #[cfg(feature = "apply_before_commit")]
    passed_on: mpsc::UnboundedSender<Commit>,
}

type Job = (
    Arc<Engine>,
    Vec<Commit>,
    oneshot::Sender<Result<Engine, EngineAlarm>>,
);

enum Calculator {
    Thread(std::sync::mpsc::Sender<Job>),
    Inline(Duration),
}

type Calculating = Pin<Box<dyn Future<Output = Result<Engine, EngineAlarm>> + Send>>;

impl Owner {
    pub fn new(engine: Engine, settings: Settings) -> (Owner, Handle) {
        let (requests, waiting) = mpsc::channel(WAITING_REQUESTS);
        let (passed_on, commits) = mpsc::unbounded_channel();
        let committed_count = Arc::new(AtomicU64::new(engine.next.0));
        let (published, engines) = watch::channel(Arc::new(engine));
        let handle = Handle {
            requests,
            published: engines,
            committed_count: Arc::clone(&committed_count),
            busy_after: settings.busy_after,
            #[cfg(feature = "apply_before_commit")]
            passed_on: passed_on.clone(),
        };
        let owner = Owner {
            calculation: settings.calculation,
            requests: waiting,
            passed_on,
            commits,
            published,
            committed_count,
        };
        (owner, handle)
    }

    // shortcut: every new owner rebuilds the engine from the start of the log; add snapshots when start-up time matters.
    pub async fn run(self, store: Arc<dyn ObjectStore>, workspace: &str, epoch: Epoch) -> Exit {
        let Owner {
            calculation,
            requests,
            passed_on,
            commits,
            published,
            committed_count,
        } = self;
        let engine = Arc::clone(&published.borrow());
        let definitions = engine.definitions.clone();
        let calculator = match calculation {
            Calculation::Threads(threads) => Calculator::Thread(engine_thread(threads)),
            Calculation::Inline(cost) => Calculator::Inline(cost),
        };
        // Not two spawned tasks: a crash aborts this one future, so the engine stops with the owner. The engine thread can finish its batch, but its result is dropped.
        tokio::select! {
            biased;
            exit = change_log::serve(store, workspace, epoch, definitions, requests, passed_on) => {
                Exit::ChangeLog(exit)
            }
            alarm = apply(engine, commits, &calculator, &published, &committed_count) => {
                Exit::Engine(alarm)
            }
        }
    }
}

async fn apply(
    engine: Arc<Engine>,
    mut commits: mpsc::UnboundedReceiver<Commit>,
    calculator: &Calculator,
    published: &watch::Sender<Arc<Engine>>,
    committed_count: &AtomicU64,
) -> EngineAlarm {
    let mut applier = Applier::new(engine);
    let mut calculation = None;
    loop {
        let events = tokio::select! {
            biased;
            Some(commit) = commits.recv() => {
                committed_count.fetch_max(commit.sequence.0 + 1, Ordering::SeqCst);
                let mut received = vec![commit];
                if cfg!(feature = "skip_commit") {
                    received.retain(|commit| commit.sequence != Sequence(1));
                }
                if cfg!(feature = "swap_commits") && let Ok(later) = commits.try_recv() {
                    received.insert(0, later);
                }
                if cfg!(feature = "apply_twice") {
                    received.extend(received.clone());
                }
                received.into_iter().map(Event::Committed).collect()
            }
            result = async { calculation.as_mut().expect("a calculation runs").await }, if calculation.is_some() => {
                calculation = None;
                vec![Event::Finished(result)]
            }
            else => std::future::pending().await,
        };
        for event in events {
            for effect in applier.step(event) {
                match effect {
                    Effect::Calculate { engine, commits } => {
                        calculation = Some(calculator.calculate(engine, commits));
                    }
                    Effect::Publish(engine) => {
                        published.send_replace(engine);
                    }
                    Effect::Alarm(alarm) => return alarm,
                }
            }
        }
    }
}

fn engine_thread(threads: NonZeroUsize) -> std::sync::mpsc::Sender<Job> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.get())
        .build()
        .expect("the engine pool starts");
    let (jobs, received) = std::sync::mpsc::channel::<Job>();
    std::thread::spawn(move || {
        for (engine, commits, finished) in received {
            // Not an error: the owner stopped and does not read the result.
            let _ = finished.send(pool.install(|| engine.apply_batch(&commits)));
        }
    });
    jobs
}

impl Calculator {
    fn calculate(&self, engine: Arc<Engine>, commits: Vec<Commit>) -> Calculating {
        match self {
            Calculator::Thread(jobs) => {
                let (finished, result) = oneshot::channel();
                jobs.send((engine, commits, finished))
                    .expect("the engine thread runs while the owner runs");
                Box::pin(async move {
                    result
                        .await
                        .expect("the engine thread finishes every batch")
                })
            }
            Calculator::Inline(cost) => {
                let cost = *cost;
                Box::pin(async move {
                    tokio::time::sleep(cost).await;
                    engine.apply_batch(&commits)
                })
            }
        }
    }
}

impl Handle {
    pub async fn submit(
        &self,
        request_id: RequestId,
        edit: &Edit,
    ) -> Result<Sequence, SubmitError> {
        let lag = self.lag();
        if lag.behind() > self.busy_after {
            return Err(SubmitError::Rejected(Rejection::Busy(lag)));
        }
        let payload = edit.encode();
        #[cfg(feature = "apply_before_commit")]
        let _ = self.passed_on.send(Commit {
            request_id: request_id.clone(),
            sequence: self.published.borrow().next,
            payload: payload.clone(),
        });
        let (acknowledged, ack) = oneshot::channel();
        let request = Request {
            request_id,
            payload,
        };
        self.requests
            .send(Submission {
                request,
                acknowledged,
            })
            .await
            .map_err(|_| SubmitError::Stopped)?;
        let sequence = ack
            .await
            .map_err(|_| SubmitError::Stopped)?
            .map_err(SubmitError::Rejected)?;
        self.committed_count
            .fetch_max(sequence.0 + 1, Ordering::SeqCst);
        Ok(sequence)
    }

    pub async fn read(
        &self,
        at_least: Sequence,
        cells: &[(MeasureUlid, Vec<MemberId>)],
    ) -> Result<(Sequence, Vec<Option<f64>>), ReadError> {
        let lag = self.lag();
        if lag.committed < Some(at_least) {
            return Err(ReadError::NotCommitted(lag));
        }
        let mut published = self.published.clone();
        let engine = Arc::clone(
            &*published
                .wait_for(|engine| engine.next > at_least)
                .await
                .map_err(|_| ReadError::Stopped)?,
        );
        let values = cells
            .iter()
            .map(|(measure, coordinate)| {
                engine
                    .measures
                    .iter()
                    .find(|candidate| candidate.ulid() == *measure)
                    .ok_or(ReadError::NoMeasure(*measure))?
                    .get(coordinate)
                    .map_err(ReadError::Coordinate)
            })
            .collect::<Result<_, _>>()?;
        let recalculated = engine
            .recalculated()
            .expect("the engine recalculated the sequence it waited for");
        Ok((recalculated, values))
    }

    pub fn lag(&self) -> Lag {
        // Not the committed count first: a batch that finishes between the two reads would show more recalculated than committed.
        let recalculated = self.published.borrow().recalculated();
        let committed_count = self.committed_count.load(Ordering::SeqCst);
        Lag {
            committed: committed_count.checked_sub(1).map(Sequence),
            recalculated,
        }
    }

    pub fn published(&self) -> watch::Receiver<Arc<Engine>> {
        self.published.clone()
    }
}
