mod coordinate;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use change_log::{
    AdmitAll, AssigneeId, Epoch, ProcessId, Request, RequestId, Sequence, Slot, Start, Submission,
    serve,
};
use datagen::representative::{self, Size};
use dimension::MemberIndex;
use measure::MeasureGlobalId;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use object_store::{ObjectStore, ObjectStoreExt};
use server::{
    Calculation, DecodeError, Edit, Engine, EngineAlarm, Exit, Handle, Invalid, Lag, Loading,
    ReadError, Rejection, Settings, SubmitError,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use ulid::Ulid;
use workspace::{CellError, Coordinate, Workspace};

const QUANTITY: MeasureGlobalId = MeasureGlobalId(Ulid(100));
const SALES: MeasureGlobalId = MeasureGlobalId(Ulid(200));
const CELL: [u32; 5] = [0, 1, 1, 0, 1];
const CALCULATION: Duration = Duration::from_secs(10);

struct Running {
    handle: Handle,
    exit: JoinHandle<Exit>,
}

async fn start(store: &Arc<dyn ObjectStore>, epoch: u64, settings: Settings) -> Running {
    let running = start_loading(store, epoch, settings);
    support::loaded(&running.handle).await;
    running
}

fn start_loading(store: &Arc<dyn ObjectStore>, epoch: u64, settings: Settings) -> Running {
    let (handle, assignee) = support::start(
        support::engine(),
        Start::default(),
        settings,
        Arc::clone(store),
        epoch,
    );
    let exit = tokio::spawn(assignee);
    Running { handle, exit }
}

fn slow(busy_after: u64) -> Settings {
    Settings {
        busy_after,
        calculation: Calculation::Simulated(CALCULATION),
        ..Settings::default()
    }
}

fn store() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

fn id(name: &str) -> RequestId {
    RequestId(Bytes::from(name.to_owned()))
}

fn sequence(value: u64) -> Sequence {
    Sequence::new(value).expect("a test sequence is not 0")
}

fn quantity(value: Option<f64>) -> Edit {
    Edit {
        measure: QUANTITY,
        coordinate: coordinate::named(QUANTITY, &CELL.map(MemberIndex)),
        value,
    }
}

fn cells() -> Vec<(MeasureGlobalId, Coordinate)> {
    [QUANTITY, SALES]
        .map(|measure| (measure, coordinate::named(measure, &CELL.map(MemberIndex))))
        .to_vec()
}

async fn submit_bytes(
    submission_sender: &mpsc::Sender<Submission<std::convert::Infallible>>,
    name: &str,
    payload: Bytes,
) -> Option<Sequence> {
    let (submission, ack) = Submission::new(Request {
        request_id: id(name),
        payload,
    });
    submission_sender
        .send(submission)
        .await
        .expect("the assignee is running");
    ack.await.ok()?.ok()
}

#[tokio::test(start_paused = true)]
async fn only_checked_edits_enter_the_log_and_a_newer_assignee_recalculates_them() {
    let store = store();
    let a = start(&store, 1, Settings::default()).await;
    assert_eq!(
        a.handle.submit(id("r1"), &quantity(Some(3.0))).await,
        Ok(sequence(1))
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
        Ok(sequence(2)),
        "the rejected edit took no Commit Sequence"
    );
    assert_eq!(
        a.handle.read(Some(sequence(2)), &cells()).await,
        Ok((Some(sequence(2)), vec![None, None]))
    );

    let b = start(&store, 2, Settings::default()).await;
    assert_eq!(
        b.handle.submit(id("r3"), &quantity(Some(5.0))).await,
        Ok(sequence(3))
    );
    let (recalculated, values) = b
        .handle
        .read(Some(sequence(3)), &cells())
        .await
        .expect("B reads");
    assert_eq!(recalculated, Some(sequence(3)));
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
    let assignee = start(&store(), 1, Settings::default()).await;
    let mut result = quantity(Some(1.0));
    result.measure = SALES;
    assert_eq!(
        assignee.handle.submit(id("result"), &result).await,
        Err(SubmitError::Rejected(Rejection::Invalid(
            Invalid::NotAnInput(SALES)
        )))
    );
    let mut short = quantity(Some(1.0));
    short.coordinate =
        Coordinate::try_new(short.coordinate.iter().skip(1)).expect("a part of a coordinate");
    assert_eq!(
        assignee.handle.submit(id("short"), &short).await,
        Err(SubmitError::Rejected(Rejection::Invalid(Invalid::Cell(
            CellError::Length {
                expected: 5,
                found: 4
            }
        ))))
    );
}

#[tokio::test(start_paused = true)]
async fn a_busy_assignee_rejects_new_edits_and_still_acknowledges_resends() {
    let assignee = start(&store(), 1, slow(0)).await;
    assert_eq!(
        assignee.handle.submit(id("r1"), &quantity(Some(1.0))).await,
        Ok(sequence(1))
    );
    let behind = Lag {
        committed: Some(sequence(1)),
        recalculated: None,
    };
    assert_eq!(
        assignee.handle.lag(),
        Ok(behind),
        "the ACK came before the recalculation"
    );
    assert_eq!(
        assignee.handle.submit(id("r1"), &quantity(Some(1.0))).await,
        Ok(sequence(1)),
        "a resend of a committed edit gets its ACK while the assignee is busy"
    );
    assert_eq!(
        assignee.handle.submit(id("r2"), &quantity(Some(2.0))).await,
        Err(SubmitError::Rejected(Rejection::Busy { behind: 1 }))
    );
    assert_eq!(
        assignee.handle.read(Some(sequence(1)), &[]).await,
        Ok((Some(sequence(1)), vec![]))
    );
    assert_eq!(
        assignee.handle.submit(id("r2"), &quantity(Some(2.0))).await,
        Ok(sequence(2)),
        "the engine caught up"
    );
}

#[tokio::test(start_paused = true)]
async fn edits_that_arrive_together_are_rejected_one_by_one_when_the_assignee_is_busy() {
    let assignee = start(&store(), 1, slow(1)).await;
    let edit = quantity(Some(1.0));
    let (r1, r2, r3) = tokio::join!(
        assignee.handle.submit(id("r1"), &edit),
        assignee.handle.submit(id("r2"), &edit),
        assignee.handle.submit(id("r3"), &edit),
    );
    assert_eq!((r1, r2), (Ok(sequence(1)), Ok(sequence(2))));
    assert_eq!(
        r3,
        Err(SubmitError::Rejected(Rejection::Busy { behind: 2 })),
        "r1 and r2 count before their PUT ends"
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_assignee_counts_the_commits_of_an_older_assignee_that_it_did_not_recalculate() {
    let store = store();
    let a = start(&store, 1, Settings::default()).await;
    for (number, name) in (1..).zip(["r1", "r2"]) {
        assert_eq!(
            a.handle.submit(id(name), &quantity(Some(1.0))).await,
            Ok(sequence(number))
        );
    }
    let b = start(&store, 2, slow(1)).await;
    assert_eq!(
        b.handle.submit(id("r3"), &quantity(Some(2.0))).await,
        Err(SubmitError::Rejected(Rejection::Busy { behind: 2 }))
    );
}

#[tokio::test(start_paused = true)]
async fn a_loaded_assignee_answers_a_read_of_a_sequence_that_an_older_assignee_committed() {
    let store = store();
    let a = start(&store, 1, Settings::default()).await;
    for (number, name) in (1..).zip(["r1", "r2"]) {
        assert_eq!(
            a.handle.submit(id(name), &quantity(Some(1.0))).await,
            Ok(sequence(number))
        );
    }
    let b = start(&store, 2, Settings::default()).await;
    let read = b.handle.read(Some(sequence(2)), &cells()).await;
    assert_eq!(
        read.map(|(recalculated, _)| recalculated),
        Ok(Some(sequence(2)))
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_assignee_recalculates_the_whole_log_in_one_batch() {
    let store = store();
    let a = start(&store, 1, Settings::default()).await;
    for (number, name) in (1..).zip(["r1", "r2", "r3"]) {
        assert_eq!(
            a.handle.submit(id(name), &quantity(Some(1.0))).await,
            Ok(sequence(number))
        );
    }
    let started = Instant::now();
    let b = start(&store, 2, slow(1000)).await;
    b.handle
        .engine_receiver()
        .expect("start gives a started assignee")
        .wait_for(|engine| engine.recalculated() == Some(sequence(3)))
        .await
        .expect("B recalculates the log");
    assert_eq!(started.elapsed(), CALCULATION);
}

#[tokio::test(start_paused = true)]
async fn a_read_without_a_sequence_returns_the_published_values_and_does_not_wait() {
    let assignee = start(&store(), 1, slow(1000)).await;
    let seeded = assignee.handle.read(None, &cells()).await;
    assert!(matches!(seeded, Ok((None, _))), "{seeded:?}");
    assert_eq!(
        assignee.handle.submit(id("r1"), &quantity(None)).await,
        Ok(sequence(1))
    );
    let started = Instant::now();
    assert_eq!(
        assignee.handle.read(None, &cells()).await,
        seeded,
        "the values from before r1"
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(
        assignee.handle.read(Some(sequence(1)), &cells()).await,
        Ok((Some(sequence(1)), vec![None, None]))
    );
    assert_eq!(started.elapsed(), CALCULATION);
}

#[tokio::test(start_paused = true)]
async fn every_call_is_loading_until_the_assignee_has_read_the_log() {
    let listing = Duration::from_secs(3);
    let config = ThrottleConfig {
        wait_list_per_call: listing,
        ..ThrottleConfig::default()
    };
    let store: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(InMemory::new(), config));
    let assignee = start_loading(&store, 1, Settings::default());
    let started = Instant::now();
    assert_eq!(assignee.handle.lag(), Err(Loading));
    assert_eq!(
        assignee.handle.read(None, &cells()).await,
        Err(ReadError::Loading)
    );
    assert_eq!(
        assignee.handle.read(Some(sequence(1)), &[]).await,
        Err(ReadError::Loading)
    );
    assert_eq!(
        assignee.handle.submit(id("r1"), &quantity(None)).await,
        Err(SubmitError::Loading)
    );
    assert!(matches!(assignee.handle.definitions(), Err(Loading)));
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "no call waits for the log"
    );
    support::loaded(&assignee.handle).await;
    assert_eq!(started.elapsed(), listing);
    assert_eq!(
        assignee.handle.read(Some(sequence(1)), &[]).await,
        Err(ReadError::NotCommitted(Lag {
            committed: None,
            recalculated: None
        }))
    );
}

#[tokio::test(start_paused = true)]
async fn an_assignee_that_stops_before_it_has_read_the_log_is_stopped() {
    let store = store();
    let _newer = start(&store, 2, Settings::default()).await;
    let older = start_loading(&store, 1, Settings::default());
    let exit = older.exit.await.expect("the older assignee ends");
    assert!(matches!(exit, Exit::ChangeLog(_)), "{exit:?}");
    assert_eq!(older.handle.lag(), Err(Loading));
    assert_eq!(
        older.handle.read(None, &cells()).await,
        Err(ReadError::Stopped)
    );
    assert_eq!(
        older.handle.submit(id("r1"), &quantity(None)).await,
        Err(SubmitError::Stopped)
    );
}

#[tokio::test(start_paused = true)]
async fn a_read_waits_for_the_recalculation_and_refuses_an_uncommitted_sequence() {
    let assignee = start(&store(), 1, slow(1000)).await;
    assert_eq!(
        assignee.handle.read(Some(sequence(1)), &cells()).await,
        Err(ReadError::NotCommitted(Lag {
            committed: None,
            recalculated: None
        }))
    );
    let started = Instant::now();
    let edits = [Some(3.0), Some(4.0), None];
    for (number, (name, value)) in (1..).zip(["r1", "r2", "r3"].into_iter().zip(edits)) {
        assert_eq!(
            assignee.handle.submit(id(name), &quantity(value)).await,
            Ok(sequence(number))
        );
    }
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "no ACK waited for a recalculation"
    );
    assert_eq!(
        assignee.handle.read(Some(sequence(4)), &cells()).await,
        Err(ReadError::NotCommitted(Lag {
            committed: Some(sequence(3)),
            recalculated: None
        }))
    );
    assert_eq!(
        assignee.handle.read(Some(sequence(2)), &cells()).await,
        Ok((Some(sequence(3)), vec![None, None])),
        "the values as of r3, a sequence after the one asked for"
    );
    assert_eq!(
        started.elapsed(),
        CALCULATION * 2,
        "r2 and r3 were recalculated in one batch"
    );
    assert_eq!(
        assignee
            .handle
            .read(
                Some(sequence(1)),
                &[(
                    SALES,
                    Coordinate::try_new(cells()[1].1.iter().take(1))
                        .expect("a part of a coordinate")
                )]
            )
            .await,
        Err(ReadError::Cell(CellError::Length {
            expected: 5,
            found: 1
        }))
    );
    let unknown = MeasureGlobalId(Ulid(999));
    assert_eq!(
        assignee
            .handle
            .read(
                Some(sequence(10)),
                &[(unknown, Coordinate::try_new([]).expect("no axis"))]
            )
            .await,
        Err(ReadError::NoMeasure(unknown)),
        "not NotCommitted: no retry can read this measure"
    );
}

#[tokio::test(start_paused = true)]
async fn a_commit_that_the_engine_cannot_apply_stops_the_assignee() {
    let store = store();
    let (submission_sender, submission_receiver) = mpsc::channel(1);
    let (commit_sender, _commit_receiver) = mpsc::unbounded_channel();
    let writer = Arc::clone(&store);
    let unchecked = tokio::spawn(async move {
        serve(
            writer,
            &support::WORKSPACE.to_string(),
            AssigneeId {
                epoch: Epoch(1),
                process: ProcessId(1),
            },
            Start::default(),
            AdmitAll,
            submission_receiver,
            commit_sender,
        )
        .await
    });
    assert_eq!(
        submit_bytes(&submission_sender, "garbage", Bytes::from_static(b"\x09")).await,
        Some(sequence(1))
    );
    unchecked.abort();

    let b = start_loading(&store, 2, Settings::default());
    assert_eq!(
        b.exit.await.expect("B ends"),
        Exit::Engine(EngineAlarm::Invalid {
            sequence: sequence(1),
            invalid: Invalid::Undecodable(DecodeError::Version(9))
        })
    );
    assert_eq!(b.handle.lag().map(|lag| lag.recalculated), Ok(None));
    assert_eq!(
        b.handle.read(Some(sequence(1)), &[]).await,
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
        .find(|measure| measure.global_id() == QUANTITY)
        .expect("the engine holds the quantity")
        .get(&CELL.map(MemberIndex))
        .expect("the cell fits")
}

async fn restored(store: &Arc<dyn ObjectStore>) -> (Engine, Start) {
    server::restore(
        store.as_ref(),
        support::WORKSPACE,
        Workspace::try_new(representative::generate(&Size::TINY, 7)).expect("a valid workspace"),
    )
    .await
    .expect("valid formulas")
}

fn snapshot_object(count: usize, name: &str) -> Path {
    Path::from(format!(
        "{}/snapshot/{count:020}/{name}",
        support::WORKSPACE
    ))
}

#[tokio::test(start_paused = true)]
async fn a_new_assignee_starts_from_the_newest_snapshot_it_can_read() {
    let store = store();
    let settings = || Settings {
        snapshot_every: 1,
        ..slow(u64::MAX)
    };
    let a = start(&store, 1, settings()).await;
    for (number, value) in (1..).zip([1.0, 2.0, 3.0]) {
        let name = format!("r{}", number - 1);
        assert_eq!(
            a.handle.submit(id(&name), &quantity(Some(value))).await,
            Ok(sequence(number))
        );
        a.handle
            .read(Some(sequence(number)), &[])
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
    let (handle, assignee) = support::start(engine, start, settings(), Arc::clone(&store), 2);
    let b = tokio::spawn(assignee);
    support::loaded(&handle).await;
    assert_eq!(
        handle
            .read(Some(sequence(3)), &cells())
            .await
            .map(|(_, values)| values[0]),
        Ok(Some(3.0)),
        "the commit after the snapshot"
    );
    assert_eq!(
        handle.submit(id("r0"), &quantity(Some(1.0))).await,
        Ok(sequence(1)),
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
    let _ = support::start(
        support::engine(),
        start,
        Settings::default(),
        Arc::new(InMemory::new()),
        1,
    );
}

#[tokio::test]
#[should_panic(expected = "the engine starts where the change log starts")]
async fn an_assignee_does_not_start_when_the_engine_and_the_log_start_at_different_slots() {
    start_from(Start {
        slot: Slot(1),
        ..Start::default()
    });
}

#[tokio::test]
#[should_panic(expected = "the engine starts where the change log starts")]
async fn an_assignee_does_not_start_when_the_engine_and_the_log_start_at_different_counts() {
    start_from(Start {
        request_ids: vec![RequestId(Bytes::from_static(b"r0"))],
        ..Start::default()
    });
}
