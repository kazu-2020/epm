use std::fmt;
use std::time::Duration;

use tokio_postgres::types::Type;
use tokio_postgres::{Client, GenericClient};
use ulid::Ulid;

#[cfg(test)]
mod mutants;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceGlobalId(pub Ulid);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Node(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Address(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replacement {
    Planned,
    Unplanned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub epoch: Epoch,
    pub address_epoch: Epoch,
    pub node: Option<Node>,
    pub address: Option<Address>,
    pub boot_duration: Option<Duration>,
    pub consecutive_failures: u32,
}

#[derive(Debug)]
pub enum Error {
    Postgres(tokio_postgres::Error),
    EpochAboveBigint(Epoch),
    NegativeColumn { column: &'static str, value: i64 },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Postgres(error) => write!(formatter, "placement: {error}"),
            Self::EpochAboveBigint(epoch) => {
                write!(
                    formatter,
                    "placement: epoch {} does not fit in a bigint",
                    epoch.0
                )
            }
            Self::NegativeColumn { column, value } => {
                write!(
                    formatter,
                    "placement: column {column} holds the negative value {value}"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Postgres(error) => Some(error),
            Self::EpochAboveBigint(_) | Self::NegativeColumn { .. } => None,
        }
    }
}

impl From<tokio_postgres::Error> for Error {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Postgres(error)
    }
}

fn bigint(epoch: Epoch) -> Result<i64, Error> {
    i64::try_from(epoch.0).map_err(|_| Error::EpochAboveBigint(epoch))
}

fn non_negative(column: &'static str, value: i64) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::NegativeColumn { column, value })
}

fn text(workspace: WorkspaceGlobalId) -> String {
    workspace.0.to_string()
}

const CREATE_ROW: &str = "INSERT INTO public.placement (workspace_global_id) VALUES ($1)";

pub async fn create_row(
    client: &impl GenericClient,
    workspace: WorkspaceGlobalId,
) -> Result<(), Error> {
    client
        .query_typed(CREATE_ROW, &[(&text(workspace), Type::TEXT)])
        .await?;
    Ok(())
}

const LOOKUP: &str = "SELECT epoch, address_epoch, node, address,
       (extract(epoch FROM boot_duration) * 1000)::bigint, consecutive_failures
FROM public.placement
WHERE workspace_global_id = $1";

pub async fn lookup(
    client: &impl GenericClient,
    workspace: WorkspaceGlobalId,
) -> Result<Option<Row>, Error> {
    let Some(columns) = client
        .query_typed_opt(LOOKUP, &[(&text(workspace), Type::TEXT)])
        .await?
    else {
        return Ok(None);
    };
    let boot_milliseconds: Option<i64> = columns.try_get(4)?;
    let consecutive_failures: i32 = columns.try_get(5)?;
    Ok(Some(Row {
        epoch: Epoch(non_negative("epoch", columns.try_get(0)?)?),
        address_epoch: Epoch(non_negative("address_epoch", columns.try_get(1)?)?),
        node: columns.try_get::<_, Option<String>>(2)?.map(Node),
        address: columns.try_get::<_, Option<String>>(3)?.map(Address),
        boot_duration: boot_milliseconds
            .map(|milliseconds| {
                non_negative("boot_duration", milliseconds).map(Duration::from_millis)
            })
            .transpose()?,
        consecutive_failures: u32::try_from(consecutive_failures).map_err(|_| {
            Error::NegativeColumn {
                column: "consecutive_failures",
                value: i64::from(consecutive_failures),
            }
        })?,
    }))
}

const ISSUE: &str = "UPDATE public.placement
SET epoch = epoch + 1, node = $2, issued_at = now(),
    consecutive_failures = CASE
      WHEN $6 THEN consecutive_failures
      WHEN address_epoch = epoch
        AND (recorded_at IS NULL OR recorded_at <= now() - interval '10 minutes') THEN 0
      ELSE consecutive_failures + 1
    END
WHERE workspace_global_id = $1 AND epoch = $3 AND address_epoch = $4
  AND (issued_at IS NULL OR issued_at + $5 * interval '1 millisecond' <= now())
  AND (NOT $6 OR address_epoch = epoch)
RETURNING epoch";

// Not impl GenericClient: inside a transaction, now() is the time of BEGIN, and a ROLLBACK issues the returned epoch again.
pub async fn issue_epoch(
    client: &Client,
    workspace: WorkspaceGlobalId,
    seen: &Row,
    node: &Node,
    wait: Duration,
    replacement: Replacement,
) -> Result<Option<Epoch>, Error> {
    issue_with(client, ISSUE, workspace, seen, node, wait, replacement).await
}

async fn issue_with(
    client: &Client,
    statement: &str,
    workspace: WorkspaceGlobalId,
    seen: &Row,
    node: &Node,
    wait: Duration,
    replacement: Replacement,
) -> Result<Option<Epoch>, Error> {
    // Not an error of this crate: PostgreSQL rejects i64::MAX milliseconds as an interval out of range.
    let wait_milliseconds = i64::try_from(wait.as_millis()).unwrap_or(i64::MAX);
    let issued = client
        .query_typed_opt(
            statement,
            &[
                (&text(workspace), Type::TEXT),
                (&node.0, Type::TEXT),
                (&bigint(seen.epoch)?, Type::INT8),
                (&bigint(seen.address_epoch)?, Type::INT8),
                (&wait_milliseconds, Type::INT8),
                (&(replacement == Replacement::Planned), Type::BOOL),
            ],
        )
        .await?;
    let Some(columns) = issued else {
        return Ok(None);
    };
    Ok(Some(Epoch(non_negative("epoch", columns.try_get(0)?)?)))
}

const RECORD: &str = "UPDATE public.placement
SET address = $4, address_epoch = $3,
    recorded_at = CASE WHEN address_epoch = $3 THEN recorded_at ELSE now() END,
    boot_duration = CASE WHEN address_epoch = $3 THEN boot_duration ELSE GREATEST(now() - issued_at, interval '0') END
WHERE workspace_global_id = $1 AND epoch = $3 AND node = $2
RETURNING (extract(epoch FROM boot_duration) * 1000)::bigint";

pub async fn record_address(
    client: &Client,
    workspace: WorkspaceGlobalId,
    epoch: Epoch,
    node: &Node,
    address: &Address,
) -> Result<Option<Duration>, Error> {
    record_with(client, RECORD, workspace, epoch, node, address).await
}

async fn record_with(
    client: &Client,
    statement: &str,
    workspace: WorkspaceGlobalId,
    epoch: Epoch,
    node: &Node,
    address: &Address,
) -> Result<Option<Duration>, Error> {
    let recorded = client
        .query_typed_opt(
            statement,
            &[
                (&text(workspace), Type::TEXT),
                (&node.0, Type::TEXT),
                (&bigint(epoch)?, Type::INT8),
                (&address.0, Type::TEXT),
            ],
        )
        .await?;
    let Some(columns) = recorded else {
        return Ok(None);
    };
    Ok(Some(Duration::from_millis(non_negative(
        "boot_duration",
        columns.try_get(0)?,
    )?)))
}

const CORRECT_AFTER_RESTORE: &str = "UPDATE public.placement
SET epoch = epoch + 4294967296, address_epoch = epoch + 4294967296,
    address = NULL, recorded_at = NULL, issued_at = NULL";

pub async fn correct_after_restore(client: &Client) -> Result<(), Error> {
    client.query_typed(CORRECT_AFTER_RESTORE, &[]).await?;
    Ok(())
}
