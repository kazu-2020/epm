use std::collections::BTreeMap;
use std::error;
use std::iter;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore, PutMode};
use tokio::sync::{mpsc, oneshot};

use crate::owner::{Alarm, Effect, Event, Owner, PutFailure, Request};
use crate::segment::{Commit, Epoch, RequestId, Segment, Sequence, Slot};

pub struct Submission {
    pub request: Request,
    pub acknowledged: oneshot::Sender<Sequence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Stopped,
    Alarm(Alarm),
    Closed,
}

// shortcut: unbounded, so a slow engine never delays an ACK; bound it by the lag of the engine once the server measures it.
pub async fn serve(
    store: Arc<dyn ObjectStore>,
    workspace: &str,
    epoch: Epoch,
    mut requests: mpsc::Receiver<Submission>,
    committed: mpsc::UnboundedSender<Commit>,
) -> Exit {
    let log = Path::from(workspace).join("log");
    let mut owner = Owner::new(epoch);
    let mut io: FuturesUnordered<BoxFuture<'static, Result<Event, Alarm>>> =
        FuturesUnordered::new();
    io.push(load(Arc::clone(&store), log.clone()).boxed());
    let mut waiters: BTreeMap<RequestId, Vec<oneshot::Sender<Sequence>>> = BTreeMap::new();
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
                    if committed.send(commit).is_err() {
                        return Exit::Closed;
                    }
                }
                Effect::Ack {
                    request_id,
                    sequence,
                } => {
                    for acknowledged in waiters.remove(&request_id).into_iter().flatten() {
                        // Not an error: a client that stopped waiting resends, and the view answers it.
                        let _ = acknowledged.send(sequence);
                    }
                }
                Effect::Stop => return Exit::Stopped,
                Effect::Alarm(alarm) => return Exit::Alarm(alarm),
            }
        }
    }
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

async fn load(store: Arc<dyn ObjectStore>, log: Path) -> Result<Event, Alarm> {
    let mut attempt = 0;
    let listed = loop {
        match store.list(Some(&log)).try_collect::<Vec<_>>().await {
            Ok(listed) => break listed,
            Err(error) => {
                permanent(&error, &log, PERMANENT_FOR_LIST)?;
                attempt += 1;
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    };
    let mut slots = BTreeMap::new();
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
        if let Some(segment) = read(store.as_ref(), &meta.location, slot).await? {
            slots.insert(slot, segment);
        }
    }
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

const PERMANENT: &[u16] = &[401, 403, 501];
// Not 404 for PUT and GET: a missing key gets 404 too. A LIST gets 404 only for a missing bucket.
const PERMANENT_FOR_LIST: &[u16] = &[401, 403, 404, 501];

fn permanent(error: &object_store::Error, path: &Path, statuses: &[u16]) -> Result<(), Alarm> {
    let status = match error {
        object_store::Error::NotImplemented { .. } => Some(501),
        _ => status(error),
    };
    match status {
        Some(status) if statuses.contains(&status) => Err(Alarm::PermanentStoreError {
            key: path.to_string(),
            status,
        }),
        _ => Ok(()),
    }
}

// Not a downcast: object_store 0.14 keeps RetryError private and wraps a failed S3 LIST in Generic.
fn status(error: &object_store::Error) -> Option<u16> {
    iter::successors(Some(error as &dyn error::Error), |error| error.source()).find_map(|error| {
        let text = error.to_string();
        let rest = text.strip_prefix("Server returned non-2xx status code: ")?;
        rest.get(..3)?.parse::<u16>().ok()
    })
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(10) * 2u32.pow(attempt.min(7))
}
