use std::time::Duration;

use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, NoTls};
use ulid::Ulid;

use super::{
    Address, Epoch, Error, ISSUE, Node, RECORD, Replacement, Row, WorkspaceGlobalId, create_row,
    issue_with, lookup, record_with, text,
};

struct Statements {
    issue: String,
    record: String,
}

#[derive(Clone, Copy)]
enum Statement {
    Issue,
    Record,
}

#[derive(Clone, Copy)]
enum Constant {
    True,
    False,
    Expression(&'static str),
}

#[derive(Clone, Copy, Debug)]
enum Check {
    IssuesFromOneLookupSucceedOnce,
    RowRecordedAfterTheLookupIsNotIssued,
    NoIssueBeforeTheDeadline,
    PlannedIssueOnABootingRowIsRefused,
    PlannedIssueKeepsTheFailureCount,
    OldEpochIsNotRecorded,
    OtherNodeOfTheSameEpochIsNotRecorded,
    IssueDoesNotChangeAnotherWorkspace,
    RecordDoesNotChangeAnotherWorkspace,
    RecordAfterTheClockMovesBackSucceeds,
}

struct Mutant {
    statement: Statement,
    before: &'static str,
    condition: &'static str,
    becomes: Constant,
    fails: Check,
}

const MUTANTS: [Mutant; 10] = [
    Mutant {
        statement: Statement::Issue,
        before: "AND ",
        condition: "epoch = $3",
        becomes: Constant::True,
        fails: Check::IssuesFromOneLookupSucceedOnce,
    },
    Mutant {
        statement: Statement::Issue,
        before: "AND ",
        condition: "address_epoch = $4",
        becomes: Constant::True,
        fails: Check::RowRecordedAfterTheLookupIsNotIssued,
    },
    Mutant {
        statement: Statement::Issue,
        before: "AND ",
        condition: "(issued_at IS NULL OR issued_at + $5 * interval '1 millisecond' <= now())",
        becomes: Constant::True,
        fails: Check::NoIssueBeforeTheDeadline,
    },
    Mutant {
        statement: Statement::Issue,
        before: "AND ",
        condition: "(NOT $6 OR address_epoch = epoch)",
        becomes: Constant::True,
        fails: Check::PlannedIssueOnABootingRowIsRefused,
    },
    Mutant {
        statement: Statement::Issue,
        before: "WHEN ",
        condition: "$6",
        becomes: Constant::False,
        fails: Check::PlannedIssueKeepsTheFailureCount,
    },
    Mutant {
        statement: Statement::Record,
        before: "AND ",
        condition: "epoch = $3",
        becomes: Constant::True,
        fails: Check::OldEpochIsNotRecorded,
    },
    Mutant {
        statement: Statement::Record,
        before: "AND ",
        condition: "node = $2",
        becomes: Constant::True,
        fails: Check::OtherNodeOfTheSameEpochIsNotRecorded,
    },
    Mutant {
        statement: Statement::Issue,
        before: "WHERE ",
        condition: "workspace_global_id = $1",
        becomes: Constant::True,
        fails: Check::IssueDoesNotChangeAnotherWorkspace,
    },
    Mutant {
        statement: Statement::Record,
        before: "WHERE ",
        condition: "workspace_global_id = $1",
        becomes: Constant::True,
        fails: Check::RecordDoesNotChangeAnotherWorkspace,
    },
    Mutant {
        statement: Statement::Record,
        before: "ELSE ",
        condition: "GREATEST(now() - issued_at, interval '0')",
        becomes: Constant::Expression("now() - issued_at"),
        fails: Check::RecordAfterTheClockMovesBackSucceeds,
    },
];

fn product() -> Statements {
    Statements {
        issue: ISSUE.to_owned(),
        record: RECORD.to_owned(),
    }
}

fn mutate(mutant: &Mutant) -> Statements {
    let target = format!("{}{}", mutant.before, mutant.condition);
    let replacement = match mutant.becomes {
        Constant::True => format!("{}TRUE", mutant.before),
        Constant::False => format!("{}FALSE", mutant.before),
        Constant::Expression(expression) => format!("{}{expression}", mutant.before),
    };
    let derive = |statement: &str| {
        assert_eq!(
            statement.matches(&target).count(),
            1,
            "{target:?} occurs once in {statement}"
        );
        statement.replace(&target, &replacement)
    };
    match mutant.statement {
        Statement::Issue => Statements {
            issue: derive(ISSUE),
            ..product()
        },
        Statement::Record => Statements {
            record: derive(RECORD),
            ..product()
        },
    }
}

async fn connect() -> Client {
    let url = std::env::var("PLACEMENT_TEST_PGBOUNCER_URL").expect(
        "PLACEMENT_TEST_PGBOUNCER_URL is the URL of PgBouncer in front of a database with the migration",
    );
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .expect("connect to PgBouncer");
    tokio::spawn(connection);
    client
}

struct Scenario<'a> {
    client: &'a Client,
    statements: &'a Statements,
    node: Node,
}

impl<'a> Scenario<'a> {
    fn new(client: &'a Client, statements: &'a Statements) -> Self {
        Self {
            client,
            statements,
            node: Node(format!("node {}", Ulid::generate())),
        }
    }

    async fn create(&self) -> Result<WorkspaceGlobalId, Error> {
        let workspace = WorkspaceGlobalId(Ulid::generate());
        create_row(self.client, workspace).await?;
        Ok(workspace)
    }

    async fn seen(&self, workspace: WorkspaceGlobalId) -> Result<Row, Error> {
        Ok(lookup(self.client, workspace)
            .await?
            .expect("the row exists"))
    }

    async fn issue_from(
        &self,
        workspace: WorkspaceGlobalId,
        seen: &Row,
        wait: Duration,
        replacement: Replacement,
    ) -> Result<Option<Epoch>, Error> {
        issue_with(
            self.client,
            &self.statements.issue,
            workspace,
            seen,
            &self.node,
            wait,
            replacement,
        )
        .await
    }

    async fn issue(&self, workspace: WorkspaceGlobalId) -> Result<Epoch, Error> {
        let seen = self.seen(workspace).await?;
        let issued = self
            .issue_from(workspace, &seen, Duration::ZERO, Replacement::Unplanned)
            .await?;
        Ok(issued.expect("the setup issues an epoch"))
    }

    async fn record(
        &self,
        workspace: WorkspaceGlobalId,
        epoch: Epoch,
        node: &Node,
    ) -> Result<Option<Duration>, Error> {
        let address = Address("n1:1".to_owned());
        record_with(
            self.client,
            &self.statements.record,
            workspace,
            epoch,
            node,
            &address,
        )
        .await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm for each check keeps a mutant next to its scenario"
    )]
    async fn holds(&self, check: Check) -> Result<bool, Error> {
        let workspace = self.create().await?;
        Ok(match check {
            Check::IssuesFromOneLookupSucceedOnce => {
                let stopped = self.seen(workspace).await?;
                self.issue_from(workspace, &stopped, Duration::ZERO, Replacement::Unplanned)
                    .await?
                    .expect("the setup issues an epoch");
                self.issue_from(workspace, &stopped, Duration::ZERO, Replacement::Unplanned)
                    .await?
                    .is_none()
            }
            Check::RowRecordedAfterTheLookupIsNotIssued => {
                let epoch = self.issue(workspace).await?;
                let booting = self.seen(workspace).await?;
                self.record(workspace, epoch, &self.node)
                    .await?
                    .expect("the setup records the address");
                self.issue_from(workspace, &booting, Duration::ZERO, Replacement::Unplanned)
                    .await?
                    .is_none()
            }
            Check::NoIssueBeforeTheDeadline => {
                self.issue(workspace).await?;
                let booting = self.seen(workspace).await?;
                self.issue_from(
                    workspace,
                    &booting,
                    Duration::from_hours(1),
                    Replacement::Unplanned,
                )
                .await?
                .is_none()
            }
            Check::PlannedIssueOnABootingRowIsRefused => {
                self.issue(workspace).await?;
                let booting = self.seen(workspace).await?;
                self.issue_from(workspace, &booting, Duration::ZERO, Replacement::Planned)
                    .await?
                    .is_none()
            }
            Check::PlannedIssueKeepsTheFailureCount => {
                self.issue(workspace).await?;
                let epoch = self.issue(workspace).await?;
                self.record(workspace, epoch, &self.node)
                    .await?
                    .expect("the setup records the address");
                let running = self.seen(workspace).await?;
                self.issue_from(workspace, &running, Duration::ZERO, Replacement::Planned)
                    .await?
                    .expect("the setup issues a planned epoch");
                self.seen(workspace).await?.consecutive_failures == running.consecutive_failures
            }
            Check::OldEpochIsNotRecorded => {
                let old = self.issue(workspace).await?;
                self.issue(workspace).await?;
                self.record(workspace, old, &self.node).await?.is_none()
            }
            Check::OtherNodeOfTheSameEpochIsNotRecorded => {
                let epoch = self.issue(workspace).await?;
                self.record(workspace, epoch, &Node("another node".to_owned()))
                    .await?
                    .is_none()
            }
            Check::IssueDoesNotChangeAnotherWorkspace => {
                let other = self.create().await?;
                let other_epoch = unique_epoch_with_room_for_corrections();
                self.client
                    .query_typed(
                        "UPDATE public.placement SET epoch = $2, address_epoch = $2 WHERE workspace_global_id = $1",
                        &[(&text(other), Type::TEXT), (&other_epoch, Type::INT8)],
                    )
                    .await?;
                let other_seen = self.seen(other).await?;
                self.issue_from(
                    workspace,
                    &other_seen,
                    Duration::ZERO,
                    Replacement::Unplanned,
                )
                .await?;
                self.seen(other).await? == other_seen
            }
            Check::RecordDoesNotChangeAnotherWorkspace => {
                let other = self.create().await?;
                let other_epoch = self.issue(other).await?;
                let other_seen = self.seen(other).await?;
                self.record(workspace, other_epoch, &self.node).await?;
                self.seen(other).await? == other_seen
            }
            Check::RecordAfterTheClockMovesBackSucceeds => {
                let epoch = self.issue(workspace).await?;
                self.client
                    .query_typed(
                        "UPDATE public.placement SET issued_at = now() + interval '1 hour' WHERE workspace_global_id = $1",
                        &[(&text(workspace), Type::TEXT)],
                    )
                    .await?;
                match self.record(workspace, epoch, &self.node).await {
                    Ok(recorded) => recorded == Some(Duration::ZERO),
                    Err(Error::Postgres(error))
                        if error.code() == Some(&SqlState::CHECK_VIOLATION) =>
                    {
                        false
                    }
                    Err(error) => return Err(error),
                }
            }
        })
    }
}

fn unique_epoch_with_room_for_corrections() -> i64 {
    i64::try_from(Ulid::generate().random() >> 32).expect("48 bits fit in a bigint")
}

async fn holds(client: &Client, statements: &Statements, check: Check) -> bool {
    Scenario::new(client, statements)
        .holds(check)
        .await
        .unwrap_or_else(|error| panic!("{check:?} did not reach its assertion: {error}"))
}

#[tokio::test]
async fn each_mutant_fails_its_check() {
    let client = connect().await;
    for mutant in &MUTANTS {
        assert!(
            holds(&client, &product(), mutant.fails).await,
            "the product fails {:?}",
            mutant.fails
        );
    }
    for mutant in &MUTANTS {
        assert!(
            !holds(&client, &mutate(mutant), mutant.fails).await,
            "{:?} passes without {:?}",
            mutant.fails,
            mutant.condition
        );
    }
}
