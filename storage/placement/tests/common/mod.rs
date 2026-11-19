use std::time::Duration;

use placement::{
    Epoch, Node, Replacement, Row, WorkspaceGlobalId, create_row, issue_epoch, lookup,
};
use tokio_postgres::{Client, NoTls};
use ulid::Ulid;

pub(crate) async fn connect() -> Client {
    let url = std::env::var("PLACEMENT_TEST_PGBOUNCER_URL").expect(
        "PLACEMENT_TEST_PGBOUNCER_URL is the URL of PgBouncer in front of a database with the migration",
    );
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .expect("connect to PgBouncer");
    tokio::spawn(connection);
    client
}

pub(crate) async fn create(client: &Client) -> WorkspaceGlobalId {
    let workspace = WorkspaceGlobalId(Ulid::generate());
    create_row(client, workspace).await.expect("create the row");
    workspace
}

pub(crate) async fn seen(client: &Client, workspace: WorkspaceGlobalId) -> Row {
    lookup(client, workspace)
        .await
        .expect("look up the row")
        .expect("the row exists")
}

pub(crate) async fn issue(client: &Client, workspace: WorkspaceGlobalId, node: &Node) -> Epoch {
    let seen = seen(client, workspace).await;
    issue_epoch(
        client,
        workspace,
        &seen,
        node,
        Duration::ZERO,
        Replacement::Unplanned,
    )
    .await
    .expect("issue an epoch")
    .expect("the lookup is current and no wait applies")
}
