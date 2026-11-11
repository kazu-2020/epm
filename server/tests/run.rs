use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use change_log::{AdmitAll, Epoch, Request, RequestId, Sequence, Slot, Start, Submission, serve};
use datagen::representative::{self, Size};
use dimension::MemberId;
use measure::{MeasureError, MeasureUlid};
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use object_store::{ObjectStore, ObjectStoreExt};
use server::{
    Calculation, DecodeError, Edit, Engine, EngineAlarm, Exit, Handle, Invalid, Lag, ReadError,
    Rejection, Settings, SubmitError,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use ulid::Ulid;

const WORKSPACE: &str = "ws~";
const QUANTITY: MeasureUlid = MeasureUlid(Ulid(100));
const SALES: MeasureUlid = MeasureUlid(Ulid(200));
const CELL: [u32; 5] = [0, 1, 1, 0, 1];
const CALCULATION: Duration = Duration::from_secs(10);

struct Running {
    handle: Handle,
    exit: JoinHandle<Exit>,
}

fn start(store: &Arc<dyn ObjectStore>, epoch: u64, settings: Settings) -> Running {
    let engine = Engine::new(representative::generate(&Size::TINY, 7)).expect("valid formulas");
    let (handle, owner) = server::start(
        engine,
        Start::default(),
        settings,
        Arc::clone(store),
        WORKSPACE.to_owned(),
        Epoch(epoch),
    );
    let exit = tokio::spawn(owner);
    Running { handle, exit }
}

fn slow(busy_after: u64) -> Settings {
    Settings {
        busy_after,
        calculation: Calculation::Inline(CALCULATION),
        ..Settings::default()
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
    let (submission, ack) = Submission::new(Request {
        request_id: id(name),
        payload,
    });
    requests
        .send(submission)
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
        Err(SubmitError::Rejected(Rejection::Invalid(
            Invalid::NotAnInput(SALES)
        )))
    );
    assert_eq!(
        a.handle.submit(id("r2"), &quantity(None)).await,
        Ok(Sequence(1)),
        "the rejected edit took no Commit Sequence"
    );
    assert_eq!(
        a.handle.read(Some(Sequence(1)), &cells()).await,
        Ok((Some(Sequence(1)), vec![None, None]))
    );

    let b = start(&store, 2, Settings::default());
    assert_eq!(
        b.handle.submit(id("r3"), &quantity(Some(5.0))).await,
        Ok(Sequence(2))
    );
    let (recalculated, values) = b
        .handle
        .read(Some(Sequence(2)), &cells())
        .await
        .expect("B reads");
    assert_eq!(recalculated, Some(Sequence(2)));
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
    let owner = start(&store(), 1, Settings::default());
    let mut result = quantity(Some(1.0));
    result.measure = SALES;
    assert_eq!(
        owner.handle.submit(id("result"), &result).await,
        Err(SubmitError::Rejected(Rejection::Invalid(
            Invalid::NotAnInput(SALES)
        )))
    );
    let mut short = quantity(Some(1.0));
    short.coordinate.pop();
    assert_eq!(
        owner.handle.submit(id("short"), &short).await,
        Err(SubmitError::Rejected(Rejection::Invalid(
            Invalid::Coordinate(MeasureError::CoordinateLength {
                expected: 5,
                found: 4
            })
        )))
    );
}

#[tokio::test(start_paused = true)]
async fn a_busy_owner_rejects_new_edits_and_still_acknowledges_resends() {
    let owner = start(&store(), 1, slow(0));
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
        Err(SubmitError::Rejected(Rejection::Busy { behind: 1 }))
    );
    assert_eq!(
        owner.handle.read(Some(Sequence(0)), &[]).await,
        Ok((Some(Sequence(0)), vec![]))
    );
    assert_eq!(
        owner.handle.submit(id("r2"), &quantity(Some(2.0))).await,
        Ok(Sequence(1)),
        "the engine caught up"
    );
}

#[tokio::test(start_paused = true)]
async fn edits_that_arrive_together_are_rejected_one_by_one_when_the_owner_is_busy() {
    let owner = start(&store(), 1, slow(1));
    let edit = quantity(Some(1.0));
    let (r1, r2, r3) = tokio::join!(
        owner.handle.submit(id("r1"), &edit),
        owner.handle.submit(id("r2"), &edit),
        owner.handle.submit(id("r3"), &edit),
    );
    assert_eq!((r1, r2), (Ok(Sequence(0)), Ok(Sequence(1))));
    assert_eq!(
        r3,
        Err(SubmitError::Rejected(Rejection::Busy { behind: 2 })),
        "r1 and r2 count before their PUT ends"
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_owner_counts_the_commits_of_an_older_owner_that_it_did_not_recalculate() {
    let store = store();
    let a = start(&store, 1, Settings::default());
    for (sequence, name) in (0..).zip(["r1", "r2"]) {
        assert_eq!(
            a.handle.submit(id(name), &quantity(Some(1.0))).await,
            Ok(Sequence(sequence))
        );
    }
    let b = start(&store, 2, slow(1));
    assert_eq!(
        b.handle.submit(id("r3"), &quantity(Some(2.0))).await,
        Err(SubmitError::Rejected(Rejection::Busy { behind: 2 }))
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_owner_waits_for_the_log_before_it_answers_a_read_of_a_committed_sequence() {
    let store = store();
    let a = start(&store, 1, Settings::default());
    for (sequence, name) in (0..).zip(["r1", "r2"]) {
        assert_eq!(
            a.handle.submit(id(name), &quantity(Some(1.0))).await,
            Ok(Sequence(sequence))
        );
    }
    let b = start(&store, 2, Settings::default());
    let read = b.handle.read(Some(Sequence(1)), &cells()).await;
    assert!(matches!(read, Ok((Some(Sequence(1)), _))), "{read:?}");
}

#[tokio::test(start_paused = true)]
async fn a_new_owner_recalculates_the_whole_log_in_one_batch() {
    let store = store();
    let a = start(&store, 1, Settings::default());
    for (sequence, name) in (0..).zip(["r1", "r2", "r3"]) {
        assert_eq!(
            a.handle.submit(id(name), &quantity(Some(1.0))).await,
            Ok(Sequence(sequence))
        );
    }
    let started = Instant::now();
    let b = start(&store, 2, slow(1000));
    b.handle
        .published()
        .wait_for(|engine| engine.recalculated() == Some(Sequence(2)))
        .await
        .expect("B recalculates the log");
    assert_eq!(started.elapsed(), CALCULATION);
}

#[tokio::test(start_paused = true)]
async fn a_read_without_a_sequence_returns_the_published_values_and_does_not_wait() {
    let owner = start(&store(), 1, slow(1000));
    let seeded = owner.handle.read(None, &cells()).await;
    assert!(matches!(seeded, Ok((None, _))), "{seeded:?}");
    assert_eq!(
        owner.handle.submit(id("r1"), &quantity(None)).await,
        Ok(Sequence(0))
    );
    let started = Instant::now();
    assert_eq!(
        owner.handle.read(None, &cells()).await,
        seeded,
        "the values from before r1"
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(
        owner.handle.read(Some(Sequence(0)), &cells()).await,
        Ok((Some(Sequence(0)), vec![None, None]))
    );
    assert_eq!(started.elapsed(), CALCULATION);
}

#[tokio::test(start_paused = true)]
async fn only_a_read_of_a_sequence_waits_for_the_log() {
    let listing = Duration::from_secs(3);
    let config = ThrottleConfig {
        wait_list_per_call: listing,
        ..ThrottleConfig::default()
    };
    let store: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(InMemory::new(), config));
    let owner = start(&store, 1, Settings::default());
    let started = Instant::now();
    let seeded = owner.handle.read(None, &cells()).await;
    assert!(matches!(seeded, Ok((None, _))), "{seeded:?}");
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(
        owner.handle.read(Some(Sequence(0)), &[]).await,
        Err(ReadError::NotCommitted(Lag {
            committed: None,
            recalculated: None
        }))
    );
    assert_eq!(started.elapsed(), listing);
}

#[tokio::test(start_paused = true)]
async fn a_read_waits_for_the_recalculation_and_refuses_an_uncommitted_sequence() {
    let owner = start(&store(), 1, slow(1000));
    assert_eq!(
        owner.handle.read(Some(Sequence(0)), &cells()).await,
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
        owner.handle.read(Some(Sequence(3)), &cells()).await,
        Err(ReadError::NotCommitted(Lag {
            committed: Some(Sequence(2)),
            recalculated: None
        }))
    );
    assert_eq!(
        owner.handle.read(Some(Sequence(1)), &cells()).await,
        Ok((Some(Sequence(2)), vec![None, None])),
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
            .read(Some(Sequence(0)), &[(SALES, vec![MemberId(0)])])
            .await,
        Err(ReadError::Coordinate(MeasureError::CoordinateLength {
            expected: 5,
            found: 1
        }))
    );
    let unknown = MeasureUlid(Ulid(999));
    assert_eq!(
        owner
            .handle
            .read(Some(Sequence(9)), &[(unknown, vec![])])
            .await,
        Err(ReadError::NoMeasure(unknown)),
        "not NotCommitted: no retry can read this measure"
    );
}

#[tokio::test(start_paused = true)]
async fn a_commit_that_the_engine_cannot_apply_stops_the_owner() {
    let store = store();
    let (requests, receiver) = mpsc::channel(1);
    let (commits, _received) = mpsc::unbounded_channel();
    let writer = Arc::clone(&store);
    let unchecked = tokio::spawn(async move {
        serve(
            writer,
            WORKSPACE,
            Epoch(1),
            Start::default(),
            AdmitAll,
            receiver,
            commits,
        )
        .await
    });
    assert_eq!(
        submit_bytes(&requests, "garbage", Bytes::from_static(b"\x09")).await,
        Some(Sequence(0))
    );
    unchecked.abort();

    let b = start(&store, 2, Settings::default());
    assert_eq!(
        b.exit.await.expect("B ends"),
        Exit::Engine(EngineAlarm::Invalid {
            sequence: Sequence(0),
            invalid: Invalid::Undecodable(DecodeError::Version(9))
        })
    );
    assert_eq!(b.handle.lag().recalculated, None);
    assert_eq!(
        b.handle.read(Some(Sequence(0)), &[]).await,
        Err(ReadError::Stopped)
    );
    assert_eq!(
        b.handle.submit(id("r1"), &quantity(Some(1.0))).await,
        Err(SubmitError::Stopped)
    );
}

fn quantity_in(engine: &Engine) -> Option<f64> {
    engine
        .measures()
        .iter()
        .find(|measure| measure.ulid() == QUANTITY)
        .expect("the engine holds the quantity")
        .get(&CELL.map(MemberId))
        .expect("the cell fits")
}

async fn restored(store: &Arc<dyn ObjectStore>) -> (Engine, Start) {
    server::restore(
        store.as_ref(),
        WORKSPACE,
        representative::generate(&Size::TINY, 7),
    )
    .await
    .expect("valid formulas")
}

fn snapshot_object(count: usize, name: &str) -> Path {
    Path::from(format!("{WORKSPACE}/snapshot/{count:020}/{name}"))
}

#[tokio::test(start_paused = true)]
async fn a_new_owner_starts_from_the_newest_snapshot_it_can_read() {
    let store = store();
    let settings = || Settings {
        snapshot_every: 1,
        ..slow(u64::MAX)
    };
    let a = start(&store, 1, settings());
    for (sequence, value) in (0..3).zip([1.0, 2.0, 3.0]) {
        let name = format!("r{sequence}");
        assert_eq!(
            a.handle.submit(id(&name), &quantity(Some(value))).await,
            Ok(Sequence(sequence))
        );
        a.handle
            .read(Some(Sequence(sequence)), &[])
            .await
            .expect("A recalculates");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    a.exit.abort();
    let (engine, start) = restored(&store).await;
    assert_eq!(start.request_ids.len(), 3);
    assert_eq!(quantity_in(&engine), Some(3.0));

    store
        .put(
            &snapshot_object(3, "requests/000000"),
            Bytes::from_static(b"broken").into(),
        )
        .await
        .expect("put");
    let (engine, start) = restored(&store).await;
    assert_eq!(
        start.request_ids,
        ["r0", "r1"].map(id),
        "the snapshot before the broken one"
    );
    assert_eq!(quantity_in(&engine), Some(2.0));
    let (handle, owner) = server::start(
        engine,
        start,
        settings(),
        Arc::clone(&store),
        WORKSPACE.to_owned(),
        Epoch(2),
    );
    let b = tokio::spawn(owner);
    assert_eq!(
        handle
            .read(Some(Sequence(2)), &cells())
            .await
            .map(|(_, values)| values[0]),
        Ok(Some(3.0)),
        "the commit after the snapshot"
    );
    assert_eq!(
        handle.submit(id("r0"), &quantity(Some(1.0))).await,
        Ok(Sequence(0)),
        "a resend of a commit in the snapshot"
    );

    b.abort();
    assert_eq!(
        restored(&store).await.1.request_ids.len(),
        3,
        "B wrote the broken snapshot again"
    );
    for count in [1, 2, 3] {
        store
            .delete(&snapshot_object(count, "manifest"))
            .await
            .expect("delete");
    }
    let (engine, start) = restored(&store).await;
    assert_eq!(start, Start::default(), "no snapshot is complete");
    assert_eq!(quantity_in(&engine), None);
}

fn start_from(start: Start) {
    let engine = Engine::new(representative::generate(&Size::TINY, 7)).expect("valid formulas");
    let _ = server::start(
        engine,
        start,
        Settings::default(),
        Arc::new(InMemory::new()),
        WORKSPACE.to_owned(),
        Epoch(1),
    );
}

#[tokio::test]
#[should_panic(expected = "the engine starts where the change log starts")]
async fn an_owner_does_not_start_when_the_engine_and_the_log_start_at_different_slots() {
    start_from(Start {
        slot: Slot(1),
        ..Start::default()
    });
}

#[tokio::test]
#[should_panic(expected = "the engine starts where the change log starts")]
async fn an_owner_does_not_start_when_the_engine_and_the_log_start_at_different_counts() {
    start_from(Start {
        request_ids: vec![RequestId(Bytes::from_static(b"r0"))],
        ..Start::default()
    });
}
