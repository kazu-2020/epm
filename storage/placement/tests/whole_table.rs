// Not in placement.rs: these statements lock or change every row, and cargo runs the test binaries one at a time.
mod common;

use common::{connect, create, issue, seen};
use placement::{Address, Node, correct_after_restore, record_address};
use tokio_postgres::error::SqlState;

const CORRECTION: u64 = 4_294_967_296;

#[tokio::test]
async fn correction_stops_every_row_and_the_next_issue_is_not_a_failure() {
    let client = connect().await;
    let node = Node("n1".to_owned());
    let (running, booting) = (create(&client).await, create(&client).await);
    issue(&client, running, &node).await;
    let epoch = issue(&client, running, &node).await;
    record_address(&client, running, epoch, &node, &Address("n1:1".to_owned()))
        .await
        .unwrap()
        .expect("the record succeeds");
    issue(&client, booting, &node).await;
    assert_eq!(seen(&client, running).await.consecutive_failures, 1);

    correct_after_restore(&client).await.unwrap();

    for (workspace, epoch_before) in [(running, 2), (booting, 1)] {
        let corrected = seen(&client, workspace).await;
        assert_eq!(
            (
                corrected.epoch.get(),
                corrected.address_epoch.get(),
                corrected.address
            ),
            (epoch_before + CORRECTION, epoch_before + CORRECTION, None)
        );
        assert_eq!(
            issue(&client, workspace, &node).await.get(),
            epoch_before + CORRECTION + 1
        );
        assert_eq!(seen(&client, workspace).await.consecutive_failures, 0);
    }
}

#[tokio::test]
async fn table_rejects_a_truncate() {
    let client = connect().await;
    let workspace = create(&client).await;

    // In a transaction: without the trigger, the ROLLBACK keeps the rows.
    let error = client
        .batch_execute("BEGIN; TRUNCATE public.placement")
        .await
        .expect_err("TRUNCATE");
    client.batch_execute("ROLLBACK").await.unwrap();

    assert_eq!(error.code(), Some(&SqlState::RESTRICT_VIOLATION));
    assert_eq!(seen(&client, workspace).await.epoch.get(), 0);
}
