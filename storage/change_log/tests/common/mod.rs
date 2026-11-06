use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use change_log::{
    Alarm, Commit, Epoch, Exit, Request, RequestId, Segment, Sequence, Slot, Submission, serve,
};
use futures_util::stream::{self, BoxStream};
use futures_util::{FutureExt, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

const SERVERS: usize = 2;
const CLIENTS: usize = 3;
const REQUESTS_PER_CLIENT: usize = 4;
const WORKLOAD: Duration = Duration::from_secs(20);
const CLIENT_WAIT: Duration = Duration::from_secs(2);
const LATEST_APPLY: Duration = Duration::from_secs(2);
const PROBE_WAIT: Duration = Duration::from_secs(60);
const LATEST_REPLY: Duration = Duration::from_secs(1);
const ACK_LOST_PERCENT: u64 = 20;

#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn percent(&mut self, chance: u64) -> bool {
        self.below(100) < chance
    }

    fn fork(&mut self) -> Rng {
        Rng(self.next())
    }

    fn up_to(&mut self, longest: Duration) -> Duration {
        let millis = u64::try_from(longest.as_millis()).expect("short durations");
        Duration::from_millis(self.below(millis + 1))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Granted {
        server: usize,
        epoch: u64,
    },
    Crashed {
        server: usize,
    },
    Exited {
        server: usize,
        epoch: u64,
        exit: Exit,
    },
    Panicked {
        server: usize,
        epoch: u64,
    },
    Sent {
        key: String,
        body: Bytes,
    },
    Applied {
        key: String,
        body: Bytes,
    },
    Rejected {
        key: String,
    },
    Acked {
        request: String,
        epoch: u64,
        sequence: Sequence,
    },
    PassedOn {
        epoch: u64,
        commit: Commit,
    },
    ProbeStarted,
    ProbeFinished {
        acknowledged: bool,
    },
}

type History = Arc<Mutex<Vec<Record>>>;

fn record(history: &History, record: Record) {
    history.lock().expect("history").push(record);
}

#[derive(Debug)]
pub struct Faults {
    inner: Arc<dyn ObjectStore>,
    rng: Mutex<Rng>,
    calm: AtomicBool,
    history: History,
}

impl fmt::Display for Faults {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Faults({})", self.inner)
    }
}

impl Faults {
    fn roll(&self) -> u64 {
        if self.calm.load(Ordering::SeqCst) {
            return 100;
        }
        self.rng.lock().expect("rng").below(100)
    }

    fn delay(&self, longest: Duration) -> Duration {
        if self.calm.load(Ordering::SeqCst) {
            return Duration::ZERO;
        }
        self.rng.lock().expect("rng").up_to(longest)
    }
}

fn unavailable(reason: &'static str) -> object_store::Error {
    object_store::Error::Generic {
        store: "Faults",
        source: reason.into(),
    }
}

async fn apply(
    inner: Arc<dyn ObjectStore>,
    history: History,
    location: Path,
    payload: PutPayload,
    options: PutOptions,
) -> object_store::Result<PutResult> {
    let body = Bytes::from(payload.clone());
    let result = inner.put_opts(&location, payload, options).await;
    let applied = match &result {
        Ok(_) => true,
        Err(object_store::Error::AlreadyExists { .. }) => {
            let key = location.to_string();
            record(&history, Record::Rejected { key });
            false
        }
        // Not taken as "not applied": a real store can apply a PUT whose reply failed.
        Err(_) => read(inner.as_ref(), &location).await.as_ref() == Some(&body),
    };
    if applied {
        let key = location.to_string();
        record(&history, Record::Applied { key, body });
    }
    result
}

async fn read(store: &dyn ObjectStore, location: &Path) -> Option<Bytes> {
    store.get(location).await.ok()?.bytes().await.ok()
}

#[async_trait]
impl ObjectStore for Faults {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let key = location.to_string();
        let body = Bytes::from(payload.clone());
        record(
            &self.history,
            Record::Sent {
                key: key.clone(),
                body,
            },
        );
        let inner = Arc::clone(&self.inner);
        let history = Arc::clone(&self.history);
        let location = location.clone();
        sleep(self.delay(Duration::from_millis(100))).await;
        match self.roll() {
            0..10 => {
                sleep(self.delay(Duration::from_millis(300))).await;
                apply(inner, history, location, payload, options).await
            }
            10..16 => {
                let _ = apply(inner, history, location, payload, options).await;
                Err(unavailable("the reply was lost"))
            }
            16..22 => {
                let late = self.delay(LATEST_APPLY);
                tokio::spawn(async move {
                    sleep(late).await;
                    let _ = apply(inner, history, location, payload, options).await;
                });
                Err(unavailable("timed out before the store applied the write"))
            }
            22..27 => Err(object_store::Error::AlreadyExists {
                path: key,
                source: "409 ConditionalRequestConflict".into(),
            }),
            27..32 => Err(unavailable("503 Slow Down")),
            32..40 => {
                let result = apply(inner, history, location, payload, options).await;
                sleep(self.delay(LATEST_REPLY)).await;
                result
            }
            _ => apply(inner, history, location, payload, options).await,
        }
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        sleep(self.delay(Duration::from_millis(30))).await;
        match self.roll() {
            0..10 => sleep(self.delay(Duration::from_millis(200))).await,
            10..15 => return Err(unavailable("503 Slow Down")),
            _ => {}
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        match self.roll() {
            0..10 => stream::once(async { Err(unavailable("503 Slow Down")) }).boxed(),
            10..20 => {
                let delay = self.delay(Duration::from_secs(1));
                let listed = self.inner.list(prefix);
                stream::once(async move {
                    sleep(delay).await;
                    listed
                })
                .flatten()
                .boxed()
            }
            20..30 => {
                let delay = self.delay(Duration::from_secs(1));
                let inner = Arc::clone(&self.inner);
                let prefix = prefix.cloned();
                stream::once(async move {
                    sleep(delay).await;
                    inner.list(prefix.as_ref())
                })
                .flatten()
                .boxed()
            }
            _ => self.inner.list(prefix),
        }
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

type Granted = Option<(u64, mpsc::Sender<Submission>)>;
type Owners = Vec<watch::Receiver<Granted>>;

struct Cluster {
    store: Arc<dyn ObjectStore>,
    workspace: String,
    history: History,
    servers: Vec<watch::Sender<Granted>>,
    owners: Vec<Option<JoinHandle<()>>>,
    recorders: Vec<JoinHandle<()>>,
    epoch: u64,
}

impl Cluster {
    fn grant(&mut self, server: usize) {
        self.crash(server);
        self.epoch += 1;
        let epoch = self.epoch;
        record(&self.history, Record::Granted { server, epoch });
        let (sender, receiver) = mpsc::channel(64);
        let (committed, mut passed_on) = mpsc::unbounded_channel();
        let store = Arc::clone(&self.store);
        let workspace = self.workspace.clone();
        let history = Arc::clone(&self.history);
        let passed_on_history = Arc::clone(&self.history);
        self.recorders.push(tokio::spawn(async move {
            while let Some(commit) = passed_on.recv().await {
                record(&passed_on_history, Record::PassedOn { epoch, commit });
            }
        }));
        self.owners[server] = Some(tokio::spawn(async move {
            let owner = serve(store, &workspace, Epoch(epoch), receiver, committed);
            let ended = match AssertUnwindSafe(owner).catch_unwind().await {
                Ok(exit) => Record::Exited {
                    server,
                    epoch,
                    exit,
                },
                Err(_) => Record::Panicked { server, epoch },
            };
            record(&history, ended);
        }));
        self.servers[server].send_replace(Some((epoch, sender)));
    }

    fn crash(&mut self, server: usize) {
        if let Some(owner) = self.owners[server].take()
            && !owner.is_finished()
        {
            owner.abort();
            record(&self.history, Record::Crashed { server });
        }
        self.servers[server].send_replace(None);
    }
}

async fn submit(owner: &mpsc::Sender<Submission>, name: &str, wait: Duration) -> Option<Sequence> {
    let (acknowledged, ack) = oneshot::channel();
    let request = Request {
        request_id: RequestId(Bytes::from(name.to_owned())),
        payload: Bytes::from(format!("payload of {name}")),
    };
    owner
        .send(Submission {
            request,
            acknowledged,
        })
        .await
        .ok()?;
    timeout(wait, ack).await.ok()?.ok()
}

async fn client(index: usize, mut rng: Rng, owners: Owners, history: History, end: Instant) {
    for request in 0..REQUESTS_PER_CLIENT {
        let name = format!("c{index}r{request}");
        while Instant::now() < end {
            let server = rng.below(SERVERS as u64) as usize;
            let owner = owners[server].borrow().clone();
            let acknowledged = match owner {
                Some((epoch, owner)) => submit(&owner, &name, rng.up_to(CLIENT_WAIT))
                    .await
                    .map(|sequence| (epoch, sequence)),
                None => None,
            };
            if let Some((epoch, sequence)) = acknowledged {
                record(
                    &history,
                    Record::Acked {
                        request: name.clone(),
                        epoch,
                        sequence,
                    },
                );
                let ack_lost = rng.percent(ACK_LOST_PERCENT);
                if !ack_lost {
                    break;
                }
            }
            sleep(rng.up_to(Duration::from_millis(200))).await;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub history: Vec<Record>,
    pub store: BTreeMap<String, Bytes>,
}

pub async fn simulate(inner: Arc<dyn ObjectStore>, workspace: &str, seed: u64) -> Outcome {
    let mut rng = Rng::new(seed);
    let history = History::default();
    let faults = Arc::new(Faults {
        inner: Arc::clone(&inner),
        rng: Mutex::new(rng.fork()),
        calm: AtomicBool::new(false),
        history: Arc::clone(&history),
    });
    let (servers, owners): (Vec<_>, Owners) = (0..SERVERS).map(|_| watch::channel(None)).unzip();
    let mut cluster = Cluster {
        store: Arc::clone(&faults) as Arc<dyn ObjectStore>,
        workspace: workspace.to_owned(),
        history: Arc::clone(&history),
        servers,
        owners: (0..SERVERS).map(|_| None).collect(),
        recorders: Vec::new(),
        epoch: 0,
    };
    let end = Instant::now() + WORKLOAD;
    let clients: Vec<_> = (0..CLIENTS)
        .map(|index| {
            let run = client(index, rng.fork(), owners.clone(), Arc::clone(&history), end);
            tokio::spawn(run)
        })
        .collect();
    cluster.grant(0);
    while Instant::now() < end {
        let pause = if rng.percent(30) {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(3)
        };
        sleep(rng.up_to(pause)).await;
        let server = rng.below(SERVERS as u64) as usize;
        if rng.percent(60) {
            cluster.grant(server);
        } else {
            cluster.crash(server);
        }
    }
    for client in clients {
        client.await.expect("the client finishes");
    }
    faults.calm.store(true, Ordering::SeqCst);
    for server in 0..SERVERS {
        cluster.crash(server);
    }
    sleep(LATEST_APPLY * 2).await;

    record(&history, Record::ProbeStarted);
    let acknowledged: BTreeSet<String> = acknowledged(&history.lock().expect("history"));
    cluster.grant(0);
    let (_, owner) = cluster.servers[0]
        .borrow()
        .clone()
        .expect("the probe owner runs");
    let probe = async {
        for request in &acknowledged {
            if submit(&owner, request, PROBE_WAIT).await.is_none() {
                return false;
            }
        }
        true
    };
    let all = timeout(PROBE_WAIT, probe).await.unwrap_or(false);
    record(&history, Record::ProbeFinished { acknowledged: all });
    cluster.crash(0);
    for recorder in cluster.recorders.drain(..) {
        recorder
            .await
            .expect("a recorder ends when its owner drops the channel");
    }

    let listed: Vec<ObjectMeta> = inner
        .list(Some(&Path::from(workspace)))
        .try_collect()
        .await
        .expect("list the final store");
    let mut store = BTreeMap::new();
    for meta in listed {
        let body = read(inner.as_ref(), &meta.location)
            .await
            .expect("read the final store");
        store.insert(meta.location.to_string(), body);
    }
    let history = std::mem::take(&mut *history.lock().expect("history"));
    Outcome { history, store }
}

fn acknowledged(history: &[Record]) -> BTreeSet<String> {
    history
        .iter()
        .filter_map(|record| match record {
            Record::Acked { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn slot_of(key: &str) -> Slot {
    let name = key.rsplit('/').next().expect("a key has a name");
    Slot(name.parse().expect("a slot key"))
}

fn decode(body: &Bytes) -> Segment {
    Segment::decode(body.clone()).expect("the owner writes segments it can read")
}

fn replay(store: &BTreeMap<String, Bytes>) -> (BTreeMap<Slot, Segment>, Vec<Commit>) {
    let segments: BTreeMap<Slot, Segment> = store
        .iter()
        .map(|(key, body)| (slot_of(key), decode(body)))
        .collect();
    let log = segments
        .iter()
        .zip(0..)
        .take_while(|((slot, _), expected)| slot.0 == *expected)
        .flat_map(|((_, segment), _)| segment.commits.iter().cloned())
        .collect();
    (segments, log)
}

pub type Violations = BTreeMap<&'static str, String>;

pub fn check(outcome: &Outcome) -> Violations {
    let mut violations = Violations::new();
    let mut violate = |invariant: &'static str, detail: String| {
        violations.entry(invariant).or_insert(detail);
    };
    let mut store = BTreeMap::new();
    let mut log: Vec<Commit> = Vec::new();
    let mut written: BTreeMap<(Slot, Epoch), Bytes> = BTreeMap::new();
    let mut before_probe = None;
    let mut acknowledged: Vec<(usize, &String, u64, Sequence)> = Vec::new();
    let mut passed_on: BTreeMap<u64, Vec<(usize, &Commit)>> = BTreeMap::new();
    for (at, record) in outcome.history.iter().enumerate() {
        match record {
            Record::Sent { key, body } => {
                let first = written
                    .entry((slot_of(key), decode(body).epoch))
                    .or_insert_with(|| body.clone());
                if first != body {
                    violate("oneContentPerSlotPerEpoch", format!("record {at}: {key}"));
                }
            }
            Record::Applied { key, body } => {
                if store
                    .insert(key.clone(), body.clone())
                    .is_some_and(|old| old != *body)
                {
                    violate("segmentsNeverChange", format!("record {at}: {key} changed"));
                }
                let (segments, next) = replay(&store);
                if !segments
                    .keys()
                    .zip(0..)
                    .all(|(slot, expected)| slot.0 == expected)
                {
                    violate("slotsContiguous", format!("record {at}: hole before {key}"));
                }
                let epochs: Vec<Epoch> = segments.values().map(|segment| segment.epoch).collect();
                if !epochs.is_sorted() {
                    violate(
                        "staleEpochNeverCommits",
                        format!("record {at}: epochs {epochs:?}"),
                    );
                }
                if !next.starts_with(&log) {
                    violate(
                        "refinesChangeLog",
                        format!("record {at}: {key} took commits out of the log"),
                    );
                }
                let distinct: BTreeSet<&RequestId> =
                    next.iter().map(|commit| &commit.request_id).collect();
                if distinct.len() != next.len() {
                    violate(
                        "refinesChangeLog",
                        format!("record {at}: {key} wrote a Request ID twice"),
                    );
                }
                if !next
                    .iter()
                    .zip(0..)
                    .all(|(commit, position)| commit.sequence == Sequence(position))
                {
                    violate("commitSequenceIsPosition", format!("record {at}: {key}"));
                }
                log = next;
            }
            Record::PassedOn { epoch, commit } => {
                passed_on.entry(*epoch).or_default().push((at, commit));
            }
            Record::Acked {
                request,
                epoch,
                sequence,
            } => {
                acknowledged.push((at, request, *epoch, *sequence));
                if !log
                    .iter()
                    .any(|commit| commit.request_id.0 == request.as_bytes())
                {
                    violate(
                        "refinesChangeLog",
                        format!("record {at}: acknowledged {request} is not in the log"),
                    );
                }
            }
            Record::Exited {
                exit: Exit::Alarm(alarm),
                ..
            } => {
                let invariant = match alarm {
                    Alarm::OlderEpochAfterDataWrite { .. } => "dataWriteNeverSeesOlderEpoch",
                    Alarm::OtherContentInOwnEpoch { .. } => "oneContentPerSlotPerEpoch",
                    Alarm::UnreadableSegment { .. }
                    | Alarm::UnexpectedKey { .. }
                    | Alarm::PermanentStoreError { .. } => "noAlarm",
                };
                violate(invariant, format!("record {at}: {alarm:?}"));
            }
            Record::Panicked { server, epoch } => {
                violate(
                    "noPanic",
                    format!("record {at}: server {server} at epoch {epoch}"),
                );
            }
            Record::ProbeStarted => before_probe = Some(log.clone()),
            Record::ProbeFinished { acknowledged } => {
                if !acknowledged {
                    violate(
                        "takeoverCompletes",
                        format!("record {at}: the newest owner did not acknowledge every request"),
                    );
                }
                if before_probe.as_ref() != Some(&log) {
                    violate(
                        "newestOwnerSeesAcknowledged",
                        format!(
                            "record {at}: the newest owner wrote an acknowledged request again"
                        ),
                    );
                }
            }
            Record::Granted { .. }
            | Record::Crashed { .. }
            | Record::Exited { .. }
            | Record::Rejected { .. } => {}
        }
    }
    // Not checked against the log at the time of the record: a store over the network can show a write to a read-back before its PUT returns, and PassedOn is recorded by another task.
    let at = |sequence: Sequence| {
        usize::try_from(sequence.0)
            .ok()
            .and_then(|index| log.get(index))
    };
    for (at_record, request, epoch, sequence) in acknowledged {
        if at(sequence).is_none_or(|commit| commit.request_id.0 != request.as_bytes()) {
            violate(
                "ackCarriesSequence",
                format!("record {at_record}: {request} acknowledged as {sequence:?}"),
            );
        }
        if usize::try_from(sequence.0)
            .is_ok_and(|index| passed_on.get(&epoch).map_or(0, Vec::len) <= index)
        {
            violate(
                "passedOnBeforeAck",
                format!(
                    "record {at_record}: epoch {epoch} acknowledged {request} as {sequence:?} without passing it on"
                ),
            );
        }
    }
    for (epoch, commits) in passed_on {
        for ((at_record, commit), expected) in commits.into_iter().zip(0..) {
            if commit.sequence != Sequence(expected) || at(commit.sequence) != Some(commit) {
                violate(
                    "passedOnInLogOrder",
                    format!("record {at_record}: epoch {epoch} passed on {commit:?}"),
                );
            }
        }
    }
    if store != outcome.store {
        violate(
            "historyMatchesStore",
            "the applied writes differ from the final store".to_owned(),
        );
    }
    violations
}

pub fn counts(outcome: &Outcome) -> (usize, usize) {
    (acknowledged(&outcome.history).len(), outcome.store.len())
}

pub fn witnesses(outcome: &Outcome) -> BTreeSet<&'static str> {
    let mut seen = BTreeSet::new();
    let mut sent: BTreeMap<&str, Vec<&Bytes>> = BTreeMap::new();
    let mut epochs: BTreeMap<usize, u64> = BTreeMap::new();
    let mut applied: BTreeSet<(u64, Sequence)> = BTreeSet::new();
    let passed_on: BTreeSet<(u64, Sequence)> = outcome
        .history
        .iter()
        .filter_map(|record| match record {
            Record::PassedOn { epoch, commit } => Some((*epoch, commit.sequence)),
            _ => None,
        })
        .collect();
    for record in &outcome.history {
        match record {
            Record::Sent { key, body } => sent.entry(key).or_default().push(body),
            Record::Granted { server, epoch } => {
                epochs.insert(*server, *epoch);
            }
            Record::Applied { body, .. } => {
                let segment = decode(body);
                if let Some(first) = segment.commits.first() {
                    applied.insert((segment.epoch.0, first.sequence));
                }
            }
            Record::Crashed { server } => {
                if epochs.get(server).is_some_and(|epoch| {
                    applied
                        .iter()
                        .any(|written| written.0 == *epoch && !passed_on.contains(written))
                }) {
                    seen.insert("crashAfterApplyBeforeReply");
                }
            }
            Record::Exited {
                exit: Exit::Stopped,
                ..
            } => {
                seen.insert("oldOwnerStopped");
            }
            Record::Rejected { .. } => {
                seen.insert("storeRejectedPut");
            }
            _ => {}
        }
    }
    if acknowledged(&outcome.history).len() >= 2 {
        seen.insert("twoAcknowledged");
    }
    let (segments, _) = replay(&outcome.store);
    if segments
        .values()
        .any(|segment| segment.epoch.0 >= 2 && !segment.commits.is_empty())
    {
        seen.insert("commitAfterHandover");
    }
    if sent.values().any(|bodies| {
        bodies
            .iter()
            .enumerate()
            .any(|(i, body)| bodies[..i].contains(body))
    }) {
        seen.insert("sameBytesResent");
    }
    let fences: BTreeSet<(Epoch, Slot)> = sent
        .iter()
        .flat_map(|(key, bodies)| bodies.iter().map(|body| (slot_of(key), decode(body))))
        .filter(|(_, segment)| segment.commits.is_empty())
        .map(|(slot, segment)| (segment.epoch, slot))
        .collect();
    if fences
        .iter()
        .any(|(epoch, slot)| fences.contains(&(*epoch, Slot(slot.0 + 1))))
    {
        seen.insert("fenceMovedPastOldOwner");
    }
    seen
}
