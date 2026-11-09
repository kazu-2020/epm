use std::sync::Arc;

use bytes::Bytes;
use change_log::{AdmitAll, Epoch, Request, RequestId, Sequence, Submission, serve};
use datagen::representative::{self, Size};
use dimension::MemberId;
use measure::MeasureUlid;
use object_store::ObjectStore;
use object_store::memory::InMemory;
use server::{DecodeError, Edit, Engine, EngineAlarm, Exit, Rejection, run};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use ulid::Ulid;

const TINY: Size = Size {
    products: 2,
    customers: 2,
    departments_per_kind: 1,
    quantity_cells: 50,
    account_fill: 0.02,
};

const WORKSPACE: &str = "ws";

struct Owner {
    requests: mpsc::Sender<Submission<Rejection>>,
    recalculated: watch::Receiver<Option<Sequence>>,
    exit: JoinHandle<Exit>,
}

fn start(store: &Arc<dyn ObjectStore>, epoch: u64) -> Owner {
    let engine = Engine::new(representative::generate(&TINY, 7)).expect("valid formulas");
    let (requests, receiver) = mpsc::channel(16);
    let (sender, recalculated) = watch::channel(None);
    let store = Arc::clone(store);
    let exit =
        tokio::spawn(
            async move { run(store, WORKSPACE, Epoch(epoch), engine, receiver, sender).await },
        );
    Owner {
        requests,
        recalculated,
        exit,
    }
}

fn edit(measure: u128, value: Option<f64>) -> Bytes {
    Edit {
        measure: MeasureUlid(Ulid(measure)),
        coordinate: [0, 1, 1, 0, 1].map(MemberId).to_vec(),
        value,
    }
    .encode()
}

async fn submit<R>(
    requests: &mpsc::Sender<Submission<R>>,
    name: &str,
    payload: Bytes,
) -> Result<Result<Sequence, R>, oneshot::error::RecvError> {
    let (acknowledged, ack) = oneshot::channel();
    let request = Request {
        request_id: RequestId(Bytes::from(name.to_owned())),
        payload,
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

impl Owner {
    async fn recalculates(&mut self, sequence: u64) {
        self.recalculated
            .wait_for(|recalculated| *recalculated == Some(Sequence(sequence)))
            .await
            .expect("the owner runs");
    }
}

#[tokio::test(start_paused = true)]
async fn only_checked_edits_enter_the_log_and_a_newer_owner_recalculates_them() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut a = start(&store, 1);
    let quantity = 100;
    let sales = 200;
    assert_eq!(
        submit(&a.requests, "r1", edit(quantity, Some(3.0))).await,
        Ok(Ok(Sequence(0)))
    );
    a.recalculates(0).await;
    assert_eq!(
        submit(&a.requests, "result", edit(sales, Some(3.0))).await,
        Ok(Err(Rejection::NotAnInput(MeasureUlid(Ulid(sales))))),
        "an edit of a formula result"
    );
    assert_eq!(
        submit(&a.requests, "garbage", Bytes::from_static(b"\x09")).await,
        Ok(Err(Rejection::Undecodable(DecodeError::Version(9)))),
        "a payload that does not decode"
    );
    assert_eq!(
        submit(&a.requests, "r2", edit(quantity, None)).await,
        Ok(Ok(Sequence(1))),
        "the rejected edits took no Commit Sequence"
    );
    a.recalculates(1).await;

    let mut b = start(&store, 2);
    b.recalculates(1).await;
    assert_eq!(
        submit(&b.requests, "r3", edit(quantity, Some(5.0))).await,
        Ok(Ok(Sequence(2)))
    );
    b.recalculates(2).await;
    assert!(
        submit(&a.requests, "r4", edit(quantity, Some(1.0)))
            .await
            .is_err()
    );
    assert_eq!(
        a.exit.await.expect("A ends"),
        Exit::ChangeLog(change_log::Exit::Stopped)
    );
    drop(b.requests);
    assert_eq!(
        b.exit.await.expect("B ends"),
        Exit::ChangeLog(change_log::Exit::Closed)
    );
}

#[tokio::test(start_paused = true)]
async fn a_commit_that_the_engine_cannot_apply_stops_the_owner() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let (requests, receiver) = mpsc::channel(1);
    let (committed, _passed_on) = mpsc::unbounded_channel();
    let writer = Arc::clone(&store);
    let unchecked = tokio::spawn(async move {
        serve(writer, WORKSPACE, Epoch(1), AdmitAll, receiver, committed).await
    });
    assert_eq!(
        submit(&requests, "garbage", Bytes::from_static(b"\x09")).await,
        Ok(Ok(Sequence(0)))
    );
    unchecked.abort();

    let b = start(&store, 2);
    assert_eq!(
        b.exit.await.expect("B ends"),
        Exit::Engine(EngineAlarm::Undecodable {
            sequence: Sequence(0),
            error: DecodeError::Version(9)
        })
    );
    assert_eq!(*b.recalculated.borrow(), None);
}
