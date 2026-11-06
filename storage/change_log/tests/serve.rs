use std::sync::Arc;

use bytes::Bytes;
use change_log::{Epoch, Exit, FailPoint, Request, RequestId, Segment, Submission, serve};
use futures_util::TryStreamExt;
use object_store::memory::InMemory;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const WORKSPACE: &str = "ws";

fn start(
    store: &Arc<dyn ObjectStore>,
    epoch: u64,
    crash: impl FnMut(FailPoint) -> bool + Send + 'static,
) -> (mpsc::Sender<Submission>, JoinHandle<Exit>) {
    let (sender, receiver) = mpsc::channel(16);
    let store = Arc::clone(store);
    let owner =
        tokio::spawn(async move { serve(store, WORKSPACE, Epoch(epoch), receiver, crash).await });
    (sender, owner)
}

async fn submit(
    owner: &mpsc::Sender<Submission>,
    name: &str,
) -> Result<(), oneshot::error::RecvError> {
    let (acknowledged, ack) = oneshot::channel();
    let request = Request {
        request_id: RequestId(Bytes::from(name.to_owned())),
        payload: Bytes::from_static(b"payload"),
    };
    owner
        .send(Submission {
            request,
            acknowledged,
        })
        .await
        .expect("the owner is running");
    ack.await
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
        for commit in segment.commits {
            names.push(String::from_utf8(commit.request_id.0.to_vec()).expect("utf-8"));
        }
    }
    names
}

#[tokio::test(start_paused = true)]
async fn a_newer_owner_takes_over_and_the_old_owner_stops() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let (a, a_exit) = start(&store, 1, |_| false);
    submit(&a, "r1").await.expect("A acknowledges r1");
    submit(&a, "r2").await.expect("A acknowledges r2");
    let (b, _b_exit) = start(&store, 2, |_| false);
    submit(&b, "r1")
        .await
        .expect("B acknowledges r1 from its view");
    assert!(submit(&a, "r3").await.is_err(), "A does not acknowledge r3");
    assert_eq!(a_exit.await.expect("A ends"), Exit::Stopped);
    submit(&b, "r3").await.expect("B acknowledges r3");
    assert_eq!(log(&store).await, ["r1", "r2", "r3"]);
}

#[tokio::test(start_paused = true)]
async fn a_crash_at_each_fail_point_loses_no_acknowledged_request() {
    let cases = [
        (FailPoint::BeforePut, 3, false),
        (FailPoint::BeforeReply, 3, true),
        (FailPoint::BeforeAck, 2, true),
    ];
    for (point, nth, written) in cases {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut seen = 0;
        let (a, a_exit) = start(&store, 1, move |at| {
            seen += usize::from(at == point);
            at == point && seen == nth
        });
        submit(&a, "r0").await.expect("A acknowledges r0");
        assert!(
            submit(&a, "r1").await.is_err(),
            "{point:?}: A crashes before the ACK of r1"
        );
        assert_eq!(a_exit.await.expect("A ends"), Exit::Crashed(point));
        let expected: &[&str] = if written { &["r0", "r1"] } else { &["r0"] };
        assert_eq!(log(&store).await, expected, "{point:?}: before the resend");

        let (b, _b_exit) = start(&store, 2, |_| false);
        submit(&b, "r1")
            .await
            .expect("B acknowledges the resend of r1");
        assert_eq!(
            log(&store).await,
            ["r0", "r1"],
            "{point:?}: after the resend"
        );
    }
}
