use std::collections::BTreeMap;
use std::error;
use std::iter;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{self, BoxFuture};
use futures_util::stream::{self, FuturesUnordered};
use futures_util::{FutureExt, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore, PutMode};
use tokio::sync::{mpsc, oneshot};

use crate::owner::{Admit, Alarm, Effect, Event, Owner, PutFailure, Request, Start};
use crate::segment::{Commit, Epoch, RequestId, Segment, Sequence, Slot};

pub struct Submission<R> {
    pub request: Request,
    pub acknowledged: oneshot::Sender<Result<Sequence, R>>,
}

impl<R> Submission<R> {
    pub fn new(request: Request) -> (Self, oneshot::Receiver<Result<Sequence, R>>) {
        let (acknowledged, ack) = oneshot::channel();
        (
            Self {
                request,
                acknowledged,
            },
            ack,
        )
    }
}

type Waiters<R> = BTreeMap<RequestId, Vec<oneshot::Sender<Result<Sequence, R>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Stopped,
    Alarm(Alarm),
    Closed,
}

// Not a bounded channel for commits: a full channel would delay ACKs, and the server rejects requests as Busy while its engine is far behind.
pub async fn serve<A: Admit>(
    store: Arc<dyn ObjectStore>,
    workspace: &str,
    epoch: Epoch,
    start: Start,
    admit: A,
    mut requests: mpsc::Receiver<Submission<A::Rejection>>,
    commits: mpsc::UnboundedSender<Commit>,
) -> Exit {
    let log = Path::from(workspace).join("log");
    let first = start.slot;
    let mut owner = Owner::resume(epoch, admit, start);
    let mut io: FuturesUnordered<BoxFuture<'static, Result<Event, Alarm>>> =
        FuturesUnordered::new();
    io.push(load(Arc::clone(&store), log.clone(), first).boxed());
    let mut waiters = Waiters::new();
    loop {
        let event = tokio::select! {
            biased;
            Some(result) = io.next() => match result {
                Ok(event) => event,
                Err(alarm) => return Exit::Alarm(alarm),
            },
            submission = requests.recv() => match submission {
                Some(Submission { request, acknowledged }) => {
                    waiters.entry(request.request_id.clone()).or_default().push(acknowledged);
                    Event::Request(request)
                }
                None => return Exit::Closed,
            },
        };
        let mut effects = owner.step(event);
        effects.extend(owner.step(Event::Flush));
        for effect in effects {
            match effect {
                Effect::Put {
                    slot,
                    body,
                    attempt,
                } => {
                    io.push(put(Arc::clone(&store), key(&log, slot), slot, body, attempt).boxed());
                }
                Effect::Get { slot } => {
                    io.push(read_back(Arc::clone(&store), key(&log, slot), slot).boxed());
                }
                Effect::Committed(commit) => {
                    if commits.send(commit).is_err() {
                        return Exit::Closed;
                    }
                }
                Effect::Ack {
                    request_id,
                    sequence,
                } => acknowledge(&mut waiters, &request_id, sequence),
                Effect::Rejected { request_id, reason } => {
                    reject(&mut waiters, &request_id, reason);
                }
                Effect::Stop => return Exit::Stopped,
                Effect::Alarm(alarm) => return Exit::Alarm(alarm),
            }
        }
    }
}

fn acknowledge<R>(waiters: &mut Waiters<R>, request_id: &RequestId, sequence: Sequence) {
    for acknowledged in waiters.remove(request_id).into_iter().flatten() {
        // Not an error: a client that stopped waiting resends, and the view answers it.
        let _ = acknowledged.send(Ok(sequence));
    }
}

fn reject<R>(waiters: &mut Waiters<R>, request_id: &RequestId, reason: R) {
    let Some(waiting) = waiters.get_mut(request_id) else {
        return;
    };
    let oldest = waiting.remove(0);
    if waiting.is_empty() {
        waiters.remove(request_id);
    }
    // Not an error: a client that stopped waiting resends, and the owner checks it again.
    let _ = oldest.send(Err(reason));
}

fn key(log: &Path, slot: Slot) -> Path {
    log.clone().join(format!("{:020}", slot.0))
}

async fn put(
    store: Arc<dyn ObjectStore>,
    path: Path,
    slot: Slot,
    body: Bytes,
    attempt: u32,
) -> Result<Event, Alarm> {
    if attempt > 0 {
        tokio::time::sleep(backoff(attempt)).await;
    }
    let mode = if cfg!(feature = "blind_put") {
        PutMode::Overwrite
    } else {
        PutMode::Create
    };
    let failure = match store.put_opts(&path, body.into(), mode.into()).await {
        Ok(_) => return Ok(Event::PutSucceeded { slot }),
        Err(object_store::Error::AlreadyExists { .. }) => PutFailure::AlreadyExists,
        Err(error) => {
            permanent(&error, &path, PERMANENT)?;
            PutFailure::Unknown
        }
    };
    Ok(Event::PutFailed { slot, failure })
}

async fn read_back(store: Arc<dyn ObjectStore>, path: Path, slot: Slot) -> Result<Event, Alarm> {
    let found = read(store.as_ref(), &path, slot).await?;
    Ok(Event::ReadBack { slot, found })
}

async fn load(store: Arc<dyn ObjectStore>, log: Path, first: Slot) -> Result<Event, Alarm> {
    let mut attempt = 0;
    let listed = loop {
        let listing = match first.0.checked_sub(1) {
            Some(before) => store.list_with_offset(Some(&log), &key(&log, Slot(before))),
            None => store.list(Some(&log)),
        };
        match listing.try_collect::<Vec<_>>().await {
            Ok(listed) => break listed,
            Err(error) => {
                permanent(&error, &log, PERMANENT_FOR_LIST)?;
                attempt += 1;
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    };
    let mut keys = Vec::with_capacity(listed.len());
    for meta in listed {
        let slot = meta
            .location
            .filename()
            .and_then(|name| name.parse().ok())
            .map(Slot)
            .filter(|slot| key(&log, *slot) == meta.location)
            .ok_or_else(|| Alarm::UnexpectedKey {
                key: meta.location.to_string(),
            })?;
        keys.push((slot, meta.location));
    }
    let store = store.as_ref();
    let slots = stream::iter(keys)
        .map(|(slot, path)| async move {
            let found = read(store, &path, slot).await?;
            Ok::<_, Alarm>(found.map(|segment| (slot, segment)))
        })
        .buffer_unordered(PARALLEL_READS)
        .try_filter_map(future::ok)
        .try_collect()
        .await?;
    Ok(Event::Loaded { slots })
}

async fn read(store: &dyn ObjectStore, path: &Path, slot: Slot) -> Result<Option<Segment>, Alarm> {
    let mut attempt = 0;
    loop {
        let bytes = match store.get_opts(path, Default::default()).await {
            Ok(result) => result.bytes().await,
            Err(error) => Err(error),
        };
        match bytes {
            Ok(bytes) => {
                return Segment::decode(bytes)
                    .map(Some)
                    .map_err(|error| Alarm::UnreadableSegment { slot, error });
            }
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => {
                permanent(&error, path, PERMANENT)?;
                attempt += 1;
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

// Not one GET at a time: each S3 GET waits tens of milliseconds, and a long log would delay the fence by minutes.
const PARALLEL_READS: usize = 16;

const PERMANENT: &[u16] = &[301, 401, 403, 501];
// Not 404 for PUT and GET: a missing key gets 404 too. A LIST gets 404 only for a missing bucket.
const PERMANENT_FOR_LIST: &[u16] = &[301, 401, 403, 404, 501];

fn permanent(error: &object_store::Error, path: &Path, statuses: &[u16]) -> Result<(), Alarm> {
    let status = match error {
        object_store::Error::NotImplemented { .. } => Some(501),
        _ => status(error),
    };
    match status {
        Some(status) if statuses.contains(&status) || wrong_region(error, status) => {
            Err(Alarm::PermanentStoreError {
                key: path.to_string(),
                status,
            })
        }
        _ => Ok(()),
    }
}

// Not all of 400: S3 also sends 400 for ExpiredToken and RequestTimeout, and a retry can fix them.
fn wrong_region(error: &object_store::Error, status: u16) -> bool {
    status == 400
        && messages(error).any(|text| text.contains("<Code>AuthorizationHeaderMalformed</Code>"))
}

// Not a downcast: object_store 0.14 keeps RetryError private and wraps a failed S3 LIST in Generic.
fn status(error: &object_store::Error) -> Option<u16> {
    messages(error).find_map(|text| {
        // Not the status prefix: object_store 0.14 makes a bare 301 a BareRedirect, with no status.
        if text.starts_with("Received redirect without LOCATION") {
            return Some(301);
        }
        let rest = text.strip_prefix("Server returned non-2xx status code: ")?;
        rest.get(..3)?.parse::<u16>().ok()
    })
}

fn messages(error: &object_store::Error) -> impl Iterator<Item = String> + '_ {
    iter::successors(Some(error as &dyn error::Error), |error| error.source())
        .map(|error| error.to_string())
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(10) * 2u32.pow(attempt.min(7))
}
