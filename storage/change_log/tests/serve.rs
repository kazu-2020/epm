use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use change_log::{Commit, Epoch, Exit, Request, RequestId, Segment, Sequence, Submission, serve};
use futures_util::TryStreamExt;
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const WORKSPACE: &str = "ws";

struct Owner {
    requests: mpsc::Sender<Submission>,
    committed: mpsc::UnboundedReceiver<Commit>,
    exit: JoinHandle<Exit>,
}

fn start(store: &Arc<dyn ObjectStore>, epoch: u64) -> Owner {
    let (requests, receiver) = mpsc::channel(16);
    let (sender, committed) = mpsc::unbounded_channel();
    let store = Arc::clone(store);
    let exit =
        tokio::spawn(async move { serve(store, WORKSPACE, Epoch(epoch), receiver, sender).await });
    Owner {
        requests,
        committed,
        exit,
    }
}

impl Owner {
    fn passed_on(&mut self) -> Vec<(String, Sequence)> {
        let mut passed_on = Vec::new();
        while let Ok(commit) = self.committed.try_recv() {
            passed_on.push((name(&commit.request_id), commit.sequence));
        }
        passed_on
    }
}

async fn submit(
    requests: &mpsc::Sender<Submission>,
    name: &str,
) -> Result<Sequence, oneshot::error::RecvError> {
    let (acknowledged, ack) = oneshot::channel();
    let request = Request {
        request_id: RequestId(Bytes::from(name.to_owned())),
        payload: Bytes::from_static(b"payload"),
    };
    requests
        .send(Submission {
            request,
            acknowledged,
        })
        .await
        .expect("the owner is running");
    ack.await
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
async fn a_newer_owner_takes_over_and_the_old_owner_stops() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut a = start(&store, 1);
    assert_eq!(submit(&a.requests, "r1").await, Ok(Sequence(0)));
    assert_eq!(submit(&a.requests, "r2").await, Ok(Sequence(1)));
    assert_eq!(a.passed_on(), named(&[("r1", 0), ("r2", 1)]));
    let mut b = start(&store, 2);
    assert_eq!(
        submit(&b.requests, "r1").await,
        Ok(Sequence(0)),
        "B acknowledges r1 from its view"
    );
    assert!(
        submit(&a.requests, "r3").await.is_err(),
        "A does not acknowledge r3"
    );
    assert_eq!(a.exit.await.expect("A ends"), Exit::Stopped);
    assert_eq!(submit(&b.requests, "r3").await, Ok(Sequence(2)));
    assert_eq!(b.passed_on(), named(&[("r1", 0), ("r2", 1), ("r3", 2)]));
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
        self.inner.list(prefix)
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
        });
        let mut a = start(&store, 1);
        assert_eq!(submit(&a.requests, "r0").await, Ok(Sequence(0)));
        let requests = a.requests.clone();
        let r1 = tokio::spawn(async move { submit(&requests, "r1").await });
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        a.exit.abort();
        assert!(
            r1.await.expect("the client ends").is_err(),
            "{stall:?}: no ACK of r1"
        );
        assert_eq!(a.passed_on(), named(&[("r0", 0)]), "{stall:?}: A passed on");
        let expected: &[&str] = if written { &["r0", "r1"] } else { &["r0"] };
        assert_eq!(log(&store).await, expected, "{stall:?}: before the resend");

        let mut b = start(&store, 2);
        assert_eq!(
            submit(&b.requests, "r1").await,
            Ok(Sequence(1)),
            "{stall:?}: B acknowledges the resend"
        );
        assert_eq!(
            b.passed_on(),
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
