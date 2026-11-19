mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{connect, create, issue, seen};
use placement::{
    Address, Epoch, Error, Node, Replacement, WorkspaceGlobalId, create_row, issue_epoch, lookup,
    record_address,
};
use tokio::sync::Barrier;
use tokio::time::{sleep, timeout};
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use ulid::Ulid;

const NO_WAIT: Duration = Duration::ZERO;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

fn text(workspace: WorkspaceGlobalId) -> String {
    workspace.0.to_string()
}

fn node(name: &str) -> Node {
    Node(name.to_owned())
}

fn address() -> Address {
    Address("n1:1".to_owned())
}

async fn record(
    client: &Client,
    workspace: WorkspaceGlobalId,
    epoch: Epoch,
    node: &Node,
) -> Option<Duration> {
    record_address(client, workspace, epoch, node, &address())
        .await
        .expect("record the address")
}

async fn recorded_at(client: &Client, workspace: WorkspaceGlobalId) -> Option<String> {
    client
        .query_typed_opt(
            "SELECT recorded_at::text FROM public.placement WHERE workspace_global_id = $1",
            &[(&text(workspace), Type::TEXT)],
        )
        .await
        .expect("read recorded_at")
        .expect("the row exists")
        .get(0)
}

async fn backend_pid(client: &Client) -> i32 {
    client
        .query_typed_opt("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the backend pid")
        .expect("one row")
        .get(0)
}

async fn wait_until_blocked_by(observer: &Client, holder_pid: i32) {
    let blocked = async {
        loop {
            let waiters: i64 = observer
                .query_typed_opt(
                    "SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY (pg_blocking_pids(pid))",
                    &[(&holder_pid, Type::INT4)],
                )
                .await
                .expect("read the lock waiters")
                .expect("one row")
                .get(0);
            if waiters > 0 {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(LOCK_TIMEOUT, blocked)
        .await
        .expect("a statement waits for the row lock of the holder");
}

#[tokio::test]
async fn new_row_is_stopped_at_epoch_0() {
    let client = connect().await;
    let workspace = create(&client).await;

    let row = seen(&client, workspace).await;
    assert_eq!(
        (
            row.epoch.get(),
            row.address_epoch.get(),
            row.node,
            row.address,
            row.boot_duration,
            row.consecutive_failures
        ),
        (0, 0, None, None, None, 0)
    );
}

#[tokio::test]
async fn lookup_of_a_missing_row_is_none() {
    let client = connect().await;

    assert_eq!(
        lookup(&client, WorkspaceGlobalId(Ulid::generate()))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn create_row_rejects_a_duplicate() {
    let client = connect().await;
    let workspace = create(&client).await;

    let Err(Error::Postgres(error)) = create_row(&client, workspace).await else {
        panic!("the second row of a workspace is an error");
    };
    assert_eq!(error.code(), Some(&SqlState::UNIQUE_VIOLATION));
}

#[tokio::test]
async fn create_row_belongs_to_the_transaction_of_the_caller() {
    let mut client = connect().await;
    let workspace = WorkspaceGlobalId(Ulid::generate());

    let transaction = client.transaction().await.unwrap();
    create_row(&transaction, workspace).await.unwrap();
    transaction.rollback().await.unwrap();

    assert_eq!(lookup(&client, workspace).await.unwrap(), None);
}

#[tokio::test]
async fn first_issue_of_a_new_row_is_epoch_1_and_not_a_failure() {
    let client = connect().await;
    let workspace = create(&client).await;

    assert_eq!(issue(&client, workspace, &node("n1")).await.get(), 1);

    let row = seen(&client, workspace).await;
    assert_eq!(
        (row.epoch.get(), row.node, row.consecutive_failures),
        (1, Some(node("n1")), 0)
    );
}

#[tokio::test]
async fn issue_after_an_unrecorded_epoch_is_a_failure() {
    let client = connect().await;
    let workspace = create(&client).await;
    issue(&client, workspace, &node("n1")).await;

    assert_eq!(issue(&client, workspace, &node("n1")).await.get(), 2);

    assert_eq!(seen(&client, workspace).await.consecutive_failures, 1);
}

#[tokio::test]
async fn record_stores_the_address_and_the_boot_duration() {
    let client = connect().await;
    let workspace = create(&client).await;
    let epoch = issue(&client, workspace, &node("n1")).await;

    let boot_duration = record(&client, workspace, epoch, &node("n1")).await;

    assert!(boot_duration.is_some());
    let row = seen(&client, workspace).await;
    assert_eq!(
        (row.address_epoch, row.address, row.boot_duration),
        (epoch, Some(address()), boot_duration)
    );
    assert!(recorded_at(&client, workspace).await.is_some());
}

#[tokio::test]
async fn second_record_of_an_epoch_succeeds_and_keeps_the_first_times() {
    let client = connect().await;
    let workspace = create(&client).await;
    let epoch = issue(&client, workspace, &node("n1")).await;
    let first = record(&client, workspace, epoch, &node("n1"))
        .await
        .expect("the first record succeeds");
    let first_recorded_at = recorded_at(&client, workspace).await;
    let later = first + Duration::from_millis(50);
    sleep(later).await;

    let second = record(&client, workspace, epoch, &node("n1")).await;

    assert_eq!(
        second,
        Some(first),
        "a boot duration from the second record is at least {later:?}"
    );
    assert_eq!(recorded_at(&client, workspace).await, first_recorded_at);
}

async fn failure_count_after_a_replacement(
    client: &Client,
    workspace: WorkspaceGlobalId,
    since_the_record: &str,
) -> u32 {
    client
        .batch_execute(&format!(
            "BEGIN; UPDATE public.placement SET recorded_at = now() - interval '{since_the_record}' WHERE workspace_global_id = '{}'",
            text(workspace)
        ))
        .await
        .expect("move the record back in a transaction that fixes now()");
    issue(client, workspace, &node("n1")).await;
    let failures = seen(client, workspace).await.consecutive_failures;
    client
        .batch_execute("ROLLBACK")
        .await
        .expect("roll back the replacement");
    failures
}

#[tokio::test]
async fn replacement_within_10_minutes_of_the_record_is_a_failure() {
    let client = connect().await;
    let workspace = create(&client).await;
    let epoch = issue(&client, workspace, &node("n1")).await;
    record(&client, workspace, epoch, &node("n1"))
        .await
        .expect("the record succeeds");

    assert_eq!(
        failure_count_after_a_replacement(&client, workspace, "9 minutes 59.999999 seconds").await,
        1
    );
}

#[tokio::test]
async fn replacement_10_minutes_after_the_record_resets_the_failure_count() {
    let client = connect().await;
    let workspace = create(&client).await;
    issue(&client, workspace, &node("n1")).await;
    let epoch = issue(&client, workspace, &node("n1")).await;
    record(&client, workspace, epoch, &node("n1"))
        .await
        .expect("the record succeeds");
    assert_eq!(seen(&client, workspace).await.consecutive_failures, 1);

    assert_eq!(
        failure_count_after_a_replacement(&client, workspace, "10 minutes").await,
        0
    );
}

#[tokio::test]
async fn wait_beyond_an_interval_is_an_error_of_postgresql() {
    let client = connect().await;
    let workspace = create(&client).await;
    let stopped = seen(&client, workspace).await;

    let issued = issue_epoch(
        &client,
        workspace,
        &stopped,
        &node("n1"),
        Duration::MAX,
        Replacement::Unplanned,
    )
    .await;

    let Err(Error::Postgres(error)) = issued else {
        panic!("a wait beyond an interval is an error");
    };
    assert_eq!(error.code(), Some(&SqlState::DATETIME_FIELD_OVERFLOW));
    assert_eq!(seen(&client, workspace).await, stopped);
}

#[tokio::test]
async fn table_rejects_an_epoch_decrease_a_key_change_and_a_delete() {
    let client = connect().await;
    let workspace = create(&client).await;
    issue(&client, workspace, &node("n1")).await;
    let before = seen(&client, workspace).await;

    for statement in [
        format!(
            "UPDATE public.placement SET epoch = epoch - 1 WHERE workspace_global_id = '{}'",
            text(workspace)
        ),
        format!(
            "UPDATE public.placement SET workspace_global_id = '{}' WHERE workspace_global_id = '{}'",
            Ulid::generate(),
            text(workspace)
        ),
        format!(
            "DELETE FROM public.placement WHERE workspace_global_id = '{}'",
            text(workspace)
        ),
    ] {
        let error = client
            .batch_execute(&statement)
            .await
            .expect_err(&statement);
        assert_eq!(
            error.code(),
            Some(&SqlState::RESTRICT_VIOLATION),
            "{statement}"
        );
    }

    assert_eq!(seen(&client, workspace).await, before);
}

#[tokio::test]
async fn table_rejects_counters_out_of_range() {
    let client = connect().await;
    let workspace = create(&client).await;

    for assignment in [
        "address_epoch = epoch + 1",
        "address_epoch = -1",
        "consecutive_failures = -1",
        "boot_duration = interval '-1 second'",
    ] {
        let error = client
            .batch_execute(&format!(
                "UPDATE public.placement SET {assignment} WHERE workspace_global_id = '{}'",
                text(workspace)
            ))
            .await
            .expect_err(assignment);
        assert_eq!(
            error.code(),
            Some(&SqlState::CHECK_VIOLATION),
            "{assignment}"
        );
    }
}

#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "this test shows why the methods are disallowed"
)]
async fn named_prepared_statement_fails_through_pgbouncer() {
    let client = connect().await;
    let statement = client.prepare("SELECT 1").await.unwrap();

    let error = client
        .query(&statement, &[])
        .await
        .expect_err("PgBouncer discards the statement");

    assert_eq!(error.code(), Some(&SqlState::INVALID_SQL_STATEMENT_NAME));
}

#[tokio::test]
async fn record_of_an_old_epoch_that_waits_for_a_new_issue_is_refused() {
    let (holder, waiter, observer) = (connect().await, connect().await, connect().await);
    let workspace = create(&holder).await;
    let old = issue(&holder, workspace, &node("n1")).await;
    let booting = seen(&holder, workspace).await;
    holder.batch_execute("BEGIN").await.unwrap();
    let issued = issue_epoch(
        &holder,
        workspace,
        &booting,
        &node("n1"),
        NO_WAIT,
        Replacement::Unplanned,
    )
    .await
    .unwrap();
    assert_eq!(issued.map(Epoch::get), Some(2));
    let holder_pid = backend_pid(&holder).await;

    let recorded = tokio::spawn(async move { record(&waiter, workspace, old, &node("n1")).await });
    wait_until_blocked_by(&observer, holder_pid).await;
    holder.batch_execute("COMMIT").await.unwrap();

    assert_eq!(recorded.await.unwrap(), None);
    let row = seen(&observer, workspace).await;
    assert_eq!(
        (row.epoch.get(), row.address_epoch.get(), row.address),
        (2, 0, None)
    );
}

#[tokio::test]
async fn issue_that_waits_for_the_record_of_the_seen_epoch_is_refused() {
    let (holder, waiter, observer) = (connect().await, connect().await, connect().await);
    let workspace = create(&holder).await;
    let epoch = issue(&holder, workspace, &node("n1")).await;
    let booting = seen(&holder, workspace).await;
    holder.batch_execute("BEGIN").await.unwrap();
    assert!(
        record(&holder, workspace, epoch, &node("n1"))
            .await
            .is_some()
    );
    let holder_pid = backend_pid(&holder).await;

    let issued = tokio::spawn(async move {
        issue_epoch(
            &waiter,
            workspace,
            &booting,
            &node("n2"),
            NO_WAIT,
            Replacement::Unplanned,
        )
        .await
    });
    wait_until_blocked_by(&observer, holder_pid).await;
    holder.batch_execute("COMMIT").await.unwrap();

    assert_eq!(issued.await.unwrap().unwrap(), None);
    let row = seen(&observer, workspace).await;
    assert_eq!(
        (
            row.epoch,
            row.address_epoch,
            row.node,
            row.consecutive_failures
        ),
        (epoch, epoch, Some(node("n1")), 0)
    );
}

#[tokio::test]
async fn issue_that_waits_for_a_rolled_back_issue_succeeds_on_the_original_row() {
    let (holder, waiter, observer) = (connect().await, connect().await, connect().await);
    let workspace = create(&holder).await;
    let stopped = seen(&holder, workspace).await;
    holder.batch_execute("BEGIN").await.unwrap();
    let rolled_back = issue_epoch(
        &holder,
        workspace,
        &stopped,
        &node("n1"),
        NO_WAIT,
        Replacement::Unplanned,
    )
    .await
    .unwrap();
    assert_eq!(rolled_back.map(Epoch::get), Some(1));
    let holder_pid = backend_pid(&holder).await;

    let issued = tokio::spawn(async move {
        issue_epoch(
            &waiter,
            workspace,
            &stopped,
            &node("n2"),
            NO_WAIT,
            Replacement::Unplanned,
        )
        .await
    });
    wait_until_blocked_by(&observer, holder_pid).await;
    holder.batch_execute("ROLLBACK").await.unwrap();

    assert_eq!(issued.await.unwrap().unwrap(), rolled_back);
    let row = seen(&observer, workspace).await;
    assert_eq!(
        (row.epoch.get(), row.node, row.consecutive_failures),
        (1, Some(node("n2")), 0)
    );
}

#[tokio::test]
async fn issue_on_one_workspace_does_not_wait_for_or_change_another_workspace() {
    let (holder, other) = (connect().await, connect().await);
    let (held, free) = (create(&holder).await, create(&holder).await);
    let stopped = seen(&holder, held).await;
    holder.batch_execute("BEGIN").await.unwrap();
    let held_epoch = issue_epoch(
        &holder,
        held,
        &stopped,
        &node("n1"),
        NO_WAIT,
        Replacement::Unplanned,
    )
    .await
    .unwrap();

    let free_epochs = timeout(LOCK_TIMEOUT, async {
        (
            issue(&other, free, &node("n2")).await,
            issue(&other, free, &node("n2")).await,
        )
    })
    .await
    .expect("the row lock of one workspace does not block another workspace");
    holder.batch_execute("COMMIT").await.unwrap();

    assert_eq!(held_epoch.map(Epoch::get), Some(1));
    assert_eq!((free_epochs.0.get(), free_epochs.1.get()), (1, 2));
    assert_eq!(seen(&other, held).await.epoch.get(), 1);
    assert_eq!(seen(&other, free).await.epoch.get(), 2);
}

#[tokio::test]
async fn concurrent_issues_from_the_same_lookup_succeed_once() {
    const ISSUERS: usize = 8;
    let observer = connect().await;
    let workspace = create(&observer).await;
    let stopped = seen(&observer, workspace).await;
    let barrier = Arc::new(Barrier::new(ISSUERS));

    let mut issuers = Vec::new();
    for _ in 0..ISSUERS {
        let (client, stopped, barrier) = (connect().await, stopped.clone(), Arc::clone(&barrier));
        issuers.push(tokio::spawn(async move {
            barrier.wait().await;
            issue_epoch(
                &client,
                workspace,
                &stopped,
                &node("n1"),
                NO_WAIT,
                Replacement::Unplanned,
            )
            .await
        }));
    }
    let mut issued = Vec::new();
    for issuer in issuers {
        issued.extend(issuer.await.unwrap().unwrap());
    }

    assert_eq!(issued.into_iter().map(Epoch::get).collect::<Vec<_>>(), [1]);
    assert_eq!(seen(&observer, workspace).await.epoch.get(), 1);
}
