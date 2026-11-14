use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use change_log::{
    Admit, AdmitAll, Commit, Epoch, Exit, Request, RequestId, Segment, Sequence, Slot, Start,
    Submission, serve,
};
use futures_util::stream::{self, BoxStream};
use futures_util::{StreamExt, TryStreamExt};
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const WORKSPACE: &str = "ws";

struct Assignee {
    submission_sender: mpsc::Sender<Submission<Infallible>>,
    commit_receiver: mpsc::UnboundedReceiver<(Slot, Commit)>,
    exit: JoinHandle<Exit>,
}

fn start(store: &Arc<dyn ObjectStore>, epoch: u64) -> Assignee {
    start_from(store, epoch, Start::default())
}

fn start_from(store: &Arc<dyn ObjectStore>, epoch: u64, first: Start) -> Assignee {
    let (submission_sender, submission_receiver) = mpsc::channel(16);
    let (commit_sender, commit_receiver) = mpsc::unbounded_channel();
    let store = Arc::clone(store);
    let exit = tokio::spawn(async move {
        serve(
            store,
            WORKSPACE,
            Epoch(epoch),
            first,
            AdmitAll,
            submission_receiver,
            commit_sender,
        )
        .await
    });
    Assignee {
        submission_sender,
        commit_receiver,
        exit,
    }
}

impl Assignee {
    fn take_commits(&mut self) -> Vec<(String, Sequence)> {
        let mut named_sequences = Vec::new();
        while let Ok((_, commit)) = self.commit_receiver.try_recv() {
            named_sequences.push((name(&commit.request_id), commit.sequence));
        }
        named_sequences
    }
}

async fn submit(
    submission_sender: &mpsc::Sender<Submission<Infallible>>,
    name: &str,
) -> Result<Sequence, oneshot::error::RecvError> {
    let (submission, ack) = Submission::new(Request {
        request_id: RequestId(Bytes::from(name.to_owned())),
        payload: Bytes::from_static(b"payload"),
    });
    submission_sender
        .send(submission)
        .await
        .expect("the assignee is running");
    let Ok(sequence) = ack.await?;
    Ok(sequence)
}

fn name(request_id: &RequestId) -> String {
    String::from_utf8(request_id.0.to_vec()).expect("utf-8")
}

fn named(names: &[(&str, u64)]) -> Vec<(String, Sequence)> {
    names
        .iter()
        .map(|(name, sequence)| ((*name).to_owned(), Sequence(*sequence)))
        .collect()
}

async fn log(store: &Arc<dyn ObjectStore>) -> Vec<String> {
    let listed: Vec<_> = store
        .list(Some(&WORKSPACE.into()))
        .try_collect()
        .await
        .expect("list");
    let mut names = Vec::new();
    for meta in listed {
        let bytes = store
            .get(&meta.location)
            .await
            .expect("get")
            .bytes()
            .await
            .expect("body");
        let segment = Segment::decode(bytes).expect("segment");
        names.extend(
            segment
                .commits
                .iter()
                .map(|commit| name(&commit.request_id)),
        );
    }
    names
}

#[tokio::test(start_paused = true)]
async fn a_newer_assignee_takes_over_and_the_old_assignee_stops() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut a = start(&store, 1);
    assert_eq!(submit(&a.submission_sender, "r1").await, Ok(Sequence(0)));
    assert_eq!(submit(&a.submission_sender, "r2").await, Ok(Sequence(1)));
    assert_eq!(a.take_commits(), named(&[("r1", 0), ("r2", 1)]));
    let mut b = start(&store, 2);
    assert_eq!(
        submit(&b.submission_sender, "r1").await,
        Ok(Sequence(0)),
        "B acknowledges r1 from its view"
    );
    assert!(
        submit(&a.submission_sender, "r3").await.is_err(),
        "A does not acknowledge r3"
    );
    assert_eq!(a.exit.await.expect("A ends"), Exit::Stopped);
    assert_eq!(submit(&b.submission_sender, "r3").await, Ok(Sequence(2)));
    assert_eq!(b.take_commits(), named(&[("r1", 0), ("r2", 1), ("r3", 2)]));
    assert_eq!(log(&store).await, ["r1", "r2", "r3"]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stall {
    BeforeApply,
    AfterApply,
}

#[derive(Debug)]
struct Stalling {
    inner: InMemory,
    stall: Stall,
    put: usize,
    puts: AtomicUsize,
    listed_after: Duration,
}

impl fmt::Display for Stalling {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Stalling({:?} put {})", self.stall, self.put)
    }
}

#[async_trait]
impl ObjectStore for Stalling {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.puts.fetch_add(1, Ordering::SeqCst) != self.put {
            return self.inner.put_opts(location, payload, options).await;
        }
        if self.stall == Stall::AfterApply {
            let _ = self.inner.put_opts(location, payload, options).await;
        }
        std::future::pending().await
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
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let listed = self.inner.list(prefix);
        let listed_after = self.listed_after;
        stream::once(async move {
            tokio::time::sleep(listed_after).await;
            listed
        })
        .flatten()
        .boxed()
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

#[tokio::test(start_paused = true)]
async fn a_crash_while_a_put_waits_for_its_reply_loses_no_acknowledged_request() {
    for (stall, written) in [(Stall::BeforeApply, false), (Stall::AfterApply, true)] {
        let store: Arc<dyn ObjectStore> = Arc::new(Stalling {
            inner: InMemory::new(),
            stall,
            put: 2,
            puts: AtomicUsize::new(0),
            listed_after: Duration::ZERO,
        });
        let mut a = start(&store, 1);
        assert_eq!(submit(&a.submission_sender, "r0").await, Ok(Sequence(0)));
        let submission_sender = a.submission_sender.clone();
        let r1 = tokio::spawn(async move { submit(&submission_sender, "r1").await });
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        a.exit.abort();
        assert!(
            r1.await.expect("the client ends").is_err(),
            "{stall:?}: no ACK of r1"
        );
        assert_eq!(
            a.take_commits(),
            named(&[("r0", 0)]),
            "{stall:?}: A passed on"
        );
        let expected: &[&str] = if written { &["r0", "r1"] } else { &["r0"] };
        assert_eq!(log(&store).await, expected, "{stall:?}: before the resend");

        let mut b = start(&store, 2);
        assert_eq!(
            submit(&b.submission_sender, "r1").await,
            Ok(Sequence(1)),
            "{stall:?}: B acknowledges the resend"
        );
        assert_eq!(
            b.take_commits(),
            named(&[("r0", 0), ("r1", 1)]),
            "{stall:?}: B passed on"
        );
        assert_eq!(
            log(&store).await,
            ["r0", "r1"],
            "{stall:?}: after the resend"
        );
    }
}

struct GoodOnly;

impl Admit for GoodOnly {
    type Rejection = &'static str;

    fn admit(&mut self, payload: &Bytes) -> Result<(), &'static str> {
        if payload.as_ref() == b"bad" {
            return Err("bad");
        }
        Ok(())
    }

    fn replay(&mut self, _payload: &Bytes) {}
}

#[tokio::test(start_paused = true)]
async fn a_rejection_answers_only_the_oldest_waiter_of_its_request_id() {
    let store: Arc<dyn ObjectStore> = Arc::new(Stalling {
        inner: InMemory::new(),
        stall: Stall::BeforeApply,
        put: usize::MAX,
        puts: AtomicUsize::new(0),
        listed_after: Duration::from_secs(1),
    });
    let (submission_sender, submission_receiver) = mpsc::channel(16);
    let (commit_sender, _commit_receiver) = mpsc::unbounded_channel();
    let _assignee = tokio::spawn(async move {
        serve(
            store,
            WORKSPACE,
            Epoch(1),
            Start::default(),
            GoodOnly,
            submission_receiver,
            commit_sender,
        )
        .await
    });
    let mut answers = Vec::new();
    for payload in ["bad", "good"] {
        let (submission, ack) = Submission::new(Request {
            request_id: RequestId(Bytes::from_static(b"r1")),
            payload: Bytes::from_static(payload.as_bytes()),
        });
        submission_sender
            .send(submission)
            .await
            .expect("the assignee is running");
        answers.push(ack);
    }
    let [bad, good] = <[_; 2]>::try_from(answers).expect("two requests");
    assert_eq!(bad.await, Ok(Err("bad")));
    assert_eq!(
        good.await,
        Ok(Ok(Sequence(0))),
        "the later request with the same Request ID was committed"
    );
}

#[tokio::test(start_paused = true)]
async fn an_assignee_from_a_start_reads_no_slot_before_it() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let a = start(&store, 1);
    for (sequence, name) in (0..).zip(["r1", "r2"]) {
        assert_eq!(
            submit(&a.submission_sender, name).await,
            Ok(Sequence(sequence))
        );
    }
    a.exit.abort();
    let first_slot = Path::from(format!("{WORKSPACE}/log/{:020}", 0));
    store
        .put(&first_slot, Bytes::from_static(b"not a segment").into())
        .await
        .expect("put");

    let start = Start {
        slot: Slot(2),
        request_ids: ["r1", "r2"]
            .map(|name| RequestId(Bytes::from_static(name.as_bytes())))
            .to_vec(),
    };
    let mut b = start_from(&store, 2, start);
    assert_eq!(submit(&b.submission_sender, "r1").await, Ok(Sequence(0)));
    assert_eq!(submit(&b.submission_sender, "r3").await, Ok(Sequence(2)));
    assert_eq!(b.take_commits(), named(&[("r3", 2)]));
}
