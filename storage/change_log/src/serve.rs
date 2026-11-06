use std::collections::BTreeMap;
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
use crate::segment::{Epoch, RequestId, Segment, Slot};

pub struct Submission {
    pub request: Request,
    pub acknowledged: oneshot::Sender<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailPoint {
    BeforePut,
    BeforeReply,
    BeforeAck,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Stopped,
    Alarm(Alarm),
    Closed,
    Crashed(FailPoint),
}

pub async fn serve(
    store: Arc<dyn ObjectStore>,
    workspace: &str,
    epoch: Epoch,
    mut requests: mpsc::Receiver<Submission>,
    mut crash: impl FnMut(FailPoint) -> bool,
) -> Exit {
    let log = Path::from(workspace).join("log");
    let mut owner = Owner::new(epoch);
    let mut io: FuturesUnordered<BoxFuture<'static, Result<Event, Alarm>>> =
        FuturesUnordered::new();
    io.push(load(Arc::clone(&store), log.clone()).boxed());
    let mut waiters: BTreeMap<RequestId, Vec<oneshot::Sender<()>>> = BTreeMap::new();
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
        if matches!(event, Event::PutSucceeded { .. } | Event::PutFailed { .. })
            && crash(FailPoint::BeforeReply)
        {
            return Exit::Crashed(FailPoint::BeforeReply);
        }
        for effect in owner.step(event) {
            match effect {
                Effect::Put {
                    slot,
                    body,
                    attempt,
                } => {
                    if crash(FailPoint::BeforePut) {
                        return Exit::Crashed(FailPoint::BeforePut);
                    }
                    io.push(put(Arc::clone(&store), key(&log, slot), slot, body, attempt).boxed());
                }
                Effect::Get { slot } => {
                    io.push(read_back(Arc::clone(&store), key(&log, slot), slot).boxed());
                }
                Effect::Ack(request_id) => {
                    if crash(FailPoint::BeforeAck) {
                        return Exit::Crashed(FailPoint::BeforeAck);
                    }
                    for acknowledged in waiters.remove(&request_id).into_iter().flatten() {
                        // Not an error: a client that stopped waiting resends, and the view answers it.
                        let _ = acknowledged.send(());
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
            permanent(&error, &path)?;
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
                permanent(&error, &log)?;
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
                permanent(&error, path)?;
                attempt += 1;
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

fn permanent(error: &object_store::Error, path: &Path) -> Result<(), Alarm> {
    let status = match error {
        object_store::Error::Unauthenticated { .. } => Some(401),
        object_store::Error::PermissionDenied { .. } => Some(403),
        _ => status(error),
    };
    match status {
        Some(status @ (401 | 403 | 501)) => Err(Alarm::PermanentStoreError {
            key: path.to_string(),
            status,
        }),
        _ => Ok(()),
    }
}

// Not a downcast: object_store 0.14 keeps RetryError private and wraps a failed S3 LIST in Generic.
fn status(error: &object_store::Error) -> Option<u16> {
    std::iter::successors(Some(error as &dyn std::error::Error), |error| {
        error.source()
    })
    .find_map(|error| {
        let text = error.to_string();
        let rest = text.strip_prefix("Server returned non-2xx status code: ")?;
        rest.get(..3)?.parse().ok()
    })
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(10) * 2u32.pow(attempt.min(7))
}
