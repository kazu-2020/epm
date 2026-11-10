use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use change_log::{AdmitAll, Epoch, Request, RequestId, Sequence, Submission, serve};
use datagen::representative::{self, Size};
use dimension::MemberId;
use measure::{MeasureError, MeasureUlid};
use object_store::ObjectStore;
use object_store::memory::InMemory;
use server::{
    Calculation, DecodeError, Edit, Engine, EngineAlarm, Exit, Handle, Lag, Owner, ReadError,
    Rejection, Settings, SubmitError,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use ulid::Ulid;

const TINY: Size = Size {
    products: 2,
    customers: 2,
    departments_per_kind: 1,
    quantity_cells: 50,
    account_fill: 0.02,
};

const WORKSPACE: &str = "ws";
const QUANTITY: MeasureUlid = MeasureUlid(Ulid(100));
const SALES: MeasureUlid = MeasureUlid(Ulid(200));
const CELL: [u32; 5] = [0, 1, 1, 0, 1];
const CALCULATION: Duration = Duration::from_secs(10);

struct Running {
    handle: Handle,
    exit: JoinHandle<Exit>,
}

fn start(store: &Arc<dyn ObjectStore>, epoch: u64, settings: Settings) -> Running {
    let engine = Engine::new(representative::generate(&TINY, 7)).expect("valid formulas");
    let (owner, handle) = Owner::new(engine, settings);
    let store = Arc::clone(store);
    let exit = tokio::spawn(async move { owner.run(store, WORKSPACE, Epoch(epoch)).await });
    Running { handle, exit }
}

fn slow(busy_after: u64) -> Settings {
    Settings {
        busy_after,
        calculation: Calculation::Inline(CALCULATION),
    }
}

fn store() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

fn id(name: &str) -> RequestId {
    RequestId(Bytes::from(name.to_owned()))
}

fn quantity(value: Option<f64>) -> Edit {
    Edit {
        measure: QUANTITY,
        coordinate: CELL.map(MemberId).to_vec(),
        value,
    }
}

fn cells() -> Vec<(MeasureUlid, Vec<MemberId>)> {
    [QUANTITY, SALES]
        .map(|measure| (measure, CELL.map(MemberId).to_vec()))
        .to_vec()
}

async fn submit_bytes(
    requests: &mpsc::Sender<Submission<std::convert::Infallible>>,
    name: &str,
    payload: Bytes,
) -> Option<Sequence> {
    let (acknowledged, ack) = oneshot::channel();
    let request = Request {
        request_id: id(name),
        payload,
    };
    requests
        .send(Submission {
            request,
            acknowledged,
        })
        .await
        .expect("the owner is running");
    ack.await.ok()?.ok()
}

#[tokio::test(start_paused = true)]
async fn only_checked_edits_enter_the_log_and_a_newer_owner_recalculates_them() {
    let store = store();
    let a = start(&store, 1, Settings::default());
    assert_eq!(
        a.handle.submit(id("r1"), &quantity(Some(3.0))).await,
        Ok(Sequence(0))
    );
    let mut result = quantity(Some(3.0));
    result.measure = SALES;
    assert_eq!(
        a.handle.submit(id("result"), &result).await,
        Err(SubmitError::Rejected(Rejection::NotAnInput(SALES)))
    );
    assert_eq!(
        a.handle.submit(id("r2"), &quantity(None)).await,
        Ok(Sequence(1)),
        "the rejected edit took no Commit Sequence"
    );
    assert_eq!(
        a.handle.read(Sequence(1), &cells()).await,
        Ok((Sequence(1), vec![None, None]))
    );

    let b = start(&store, 2, Settings::default());
    assert_eq!(
        b.handle.submit(id("r3"), &quantity(Some(5.0))).await,
        Ok(Sequence(2))
    );
    let (recalculated, values) = b.handle.read(Sequence(2), &cells()).await.expect("B reads");
    assert_eq!(recalculated, Sequence(2));
    assert_eq!(values[0], Some(5.0));
    assert!(values[1].is_some(), "the sales formula sees the quantity");
    assert_eq!(
        a.handle.submit(id("r4"), &quantity(Some(1.0))).await,
        Err(SubmitError::Stopped)
    );
    assert_eq!(
        a.exit.await.expect("A ends"),
        Exit::ChangeLog(change_log::Exit::Stopped)
    );
    drop(b.handle);
    assert_eq!(
        b.exit.await.expect("B ends"),
        Exit::ChangeLog(change_log::Exit::Closed)
    );
}

#[tokio::test(start_paused = true)]
async fn a_rejected_edit_returns_its_reason() {
    let owner = start(&store(), 1, slow(0));
    let mut result = quantity(Some(1.0));
    result.measure = SALES;
    assert_eq!(
        owner.handle.submit(id("result"), &result).await,
        Err(SubmitError::Rejected(Rejection::NotAnInput(SALES)))
    );
    let mut short = quantity(Some(1.0));
    short.coordinate.pop();
    assert_eq!(
        owner.handle.submit(id("short"), &short).await,
        Err(SubmitError::Rejected(Rejection::Coordinate(
            MeasureError::CoordinateLength {
                expected: 5,
                found: 4
            }
        )))
    );
    assert_eq!(
        owner.handle.submit(id("r1"), &quantity(Some(1.0))).await,
        Ok(Sequence(0))
    );
    let behind = Lag {
        committed: Some(Sequence(0)),
        recalculated: None,
    };
    assert_eq!(
        owner.handle.lag(),
        behind,
        "the ACK came before the recalculation"
    );
    assert_eq!(
        owner.handle.submit(id("r1"), &quantity(Some(1.0))).await,
        Ok(Sequence(0)),
        "a resend of a committed edit gets its ACK while the owner is busy"
    );
    assert_eq!(
        owner.handle.submit(id("r2"), &quantity(Some(2.0))).await,
        Err(SubmitError::Rejected(Rejection::Busy(behind)))
    );
    assert_eq!(
        owner.handle.read(Sequence(0), &[]).await,
        Ok((Sequence(0), vec![]))
    );
    assert_eq!(
        owner.handle.submit(id("r2"), &quantity(Some(2.0))).await,
        Ok(Sequence(1)),
        "the engine caught up"
    );
}

#[tokio::test(start_paused = true)]
async fn a_read_waits_for_the_recalculation_and_refuses_an_uncommitted_sequence() {
    let owner = start(&store(), 1, slow(1000));
    assert_eq!(
        owner.handle.read(Sequence(0), &cells()).await,
        Err(ReadError::NotCommitted(Lag {
            committed: None,
            recalculated: None
        }))
    );
    let started = Instant::now();
    let edits = [Some(3.0), Some(4.0), None];
    for (sequence, (name, value)) in (0..).zip(["r1", "r2", "r3"].into_iter().zip(edits)) {
        assert_eq!(
            owner.handle.submit(id(name), &quantity(value)).await,
            Ok(Sequence(sequence))
        );
    }
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "no ACK waited for a recalculation"
    );
    assert_eq!(
        owner.handle.read(Sequence(3), &cells()).await,
        Err(ReadError::NotCommitted(Lag {
            committed: Some(Sequence(2)),
            recalculated: None
        }))
    );
    assert_eq!(
        owner.handle.read(Sequence(1), &cells()).await,
        Ok((Sequence(2), vec![None, None])),
        "the values as of r3, a sequence after the one asked for"
    );
    assert_eq!(
        started.elapsed(),
        CALCULATION * 2,
        "r2 and r3 were recalculated in one batch"
    );
    assert_eq!(
        owner
            .handle
            .read(Sequence(0), &[(SALES, vec![MemberId(0)])])
            .await,
        Err(ReadError::Coordinate(MeasureError::CoordinateLength {
            expected: 5,
            found: 1
        }))
    );
    let unknown = MeasureUlid(Ulid(999));
    assert_eq!(
        owner.handle.read(Sequence(0), &[(unknown, vec![])]).await,
        Err(ReadError::NoMeasure(unknown))
    );
}

#[tokio::test(start_paused = true)]
async fn a_commit_that_the_engine_cannot_apply_stops_the_owner() {
    let store = store();
    let (requests, receiver) = mpsc::channel(1);
    let (committed, _passed_on) = mpsc::unbounded_channel();
    let writer = Arc::clone(&store);
    let unchecked = tokio::spawn(async move {
        serve(writer, WORKSPACE, Epoch(1), AdmitAll, receiver, committed).await
    });
    assert_eq!(
        submit_bytes(&requests, "garbage", Bytes::from_static(b"\x09")).await,
        Some(Sequence(0))
    );
    unchecked.abort();

    let b = start(&store, 2, Settings::default());
    assert_eq!(
        b.exit.await.expect("B ends"),
        Exit::Engine(EngineAlarm::Undecodable {
            sequence: Sequence(0),
            error: DecodeError::Version(9)
        })
    );
    assert_eq!(b.handle.lag().recalculated, None);
    assert_eq!(
        b.handle.read(Sequence(0), &[]).await,
        Err(ReadError::Stopped)
    );
    assert_eq!(
        b.handle.submit(id("r1"), &quantity(Some(1.0))).await,
        Err(SubmitError::Stopped)
    );
}
