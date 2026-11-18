use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use buffa_types::google::protobuf::Value;
use bytes::Bytes;
use change_log::{Epoch, RequestId, Sequence};
use connectrpc::interceptor::{Interceptor, Next, UnaryRequest, UnaryResponse};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use dimension::{InvalidGlobalId, MemberIndex};
use measure::MeasureGlobalId;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use workspace::{Coordinate, WorkspaceGlobalId};

use crate::{Edit, Exit, Handle, Invalid, Loading, ReadError, Rejection, SubmitError};

#[expect(
    unreachable_pub,
    missing_debug_implementations,
    rust_2018_idioms,
    clippy::pedantic,
    clippy::allow_attributes,
    clippy::allow_attributes_without_reason,
    clippy::as_conversions,
    reason = "connectrpc-build generates this code, and the lints of this crate are for the code that people write"
)]
mod proto {
    connectrpc::include_generated!();
}

use proto::epm::v1::{
    AxisMember, Dimension, GetLagRequest, GetLagResponse, ListDefinitionsRequest,
    ListDefinitionsResponse, Measure, MeasureKind, Member, ReadCellsRequest, ReadCellsResponse,
    SubmitEditRequest, SubmitEditResponse, Workspace,
};

pub const WORKSPACE_HEADER: &str = "epm-workspace";
pub const EPOCH_HEADER: &str = "epm-epoch";

// Not only the body limit: the change log keeps every Request ID, and each new assignee reads them all again.
const MAXIMUM_REQUEST_ID_BYTES: usize = 128;

struct Destination {
    workspace: WorkspaceGlobalId,
    epoch: Epoch,
}

#[derive(Debug)]
pub enum Shutdown {
    Drained,
    Deadline,
    Assignee(Exit),
}

pub fn router(handle: Handle) -> Router {
    let destination = Destination {
        workspace: handle.workspace,
        epoch: handle.epoch,
    };
    let service = connectrpc::Router::new()
        .add_service(Arc::new(handle))
        .into_axum_service()
        .with_interceptor(destination);
    Router::new().fallback_service(service)
}

pub async fn serve(
    listener: TcpListener,
    handle: Handle,
    assignee: impl Future<Output = Exit> + Send + 'static,
    shutdown: impl Future<Output = ()> + Send + 'static,
    deadline: Duration,
) -> Shutdown {
    let (signal_sender, signal_receiver) = oneshot::channel();
    let shutdown = async move {
        shutdown.await;
        let _ = signal_sender.send(());
    };
    let deadline_passed = async move {
        let _ = signal_receiver.await;
        tokio::time::sleep(deadline).await;
    };
    // Not the future itself in this select: restore calculates without an await, and the accept loop must answer during it.
    let mut assignee = tokio::spawn(assignee);
    let stopped = tokio::select! {
        exit = &mut assignee => Shutdown::Assignee(
            exit.unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic())),
        ),
        served = axum::serve(listener, router(handle)).with_graceful_shutdown(shutdown) => {
            served.expect("the graceful serve of axum 0.8 returns only Ok");
            Shutdown::Drained
        }
        () = deadline_passed => Shutdown::Deadline,
    };
    assignee.abort();
    stopped
}

#[expect(
    refining_impl_trait,
    reason = "a concrete message is easier to read than the impl Encodable return type, and the generated trait accepts it"
)]
impl Workspace for Handle {
    async fn submit_edit(
        &self,
        _context: RequestContext,
        request: ServiceRequest<'_, SubmitEditRequest>,
    ) -> ServiceResult<SubmitEditResponse> {
        let request = request.to_owned_message();
        if request.request_id.is_empty() || request.request_id.len() > MAXIMUM_REQUEST_ID_BYTES {
            return Err(ConnectError::invalid_argument(format!(
                "request_id must have 1 to {MAXIMUM_REQUEST_ID_BYTES} bytes"
            )));
        }
        let edit = Edit {
            measure: measure(&request.measure)?,
            coordinate: coordinate(request.coordinate)?,
            value: edit_value(request.value.as_option())?,
        };
        let request_id = RequestId(Bytes::from(request.request_id));
        let sequence = self
            .submit(request_id, &edit)
            .await
            .map_err(submit_failure)?;
        Response::ok(SubmitEditResponse {
            sequence: Some(sequence.0),
            ..Default::default()
        })
    }

    async fn read_cells(
        &self,
        _context: RequestContext,
        request: ServiceRequest<'_, ReadCellsRequest>,
    ) -> ServiceResult<ReadCellsResponse> {
        let request = request.to_owned_message();
        let cells = request
            .cells
            .into_iter()
            .map(|cell| Ok((measure(&cell.measure)?, coordinate(cell.coordinate)?)))
            .collect::<Result<Vec<_>, ConnectError>>()?;
        let (recalculated, values) = self
            .read(request.at_least.map(Sequence), &cells)
            .await
            .map_err(read_failure)?;
        Response::ok(ReadCellsResponse {
            recalculated: recalculated.map(|sequence| sequence.0),
            values: values
                .into_iter()
                .map(|value| value.map_or_else(Value::null, Value::from))
                .collect(),
            ..Default::default()
        })
    }

    async fn list_definitions(
        &self,
        _context: RequestContext,
        _request: ServiceRequest<'_, ListDefinitionsRequest>,
    ) -> ServiceResult<ListDefinitionsResponse> {
        let engine = self.definitions().map_err(loading_failure)?;
        let measures = engine
            .measures()
            .iter()
            .map(|measure| {
                let definition = measure.definition();
                let kind = if engine.edit_check().is_input(definition.global_id()) {
                    MeasureKind::Input
                } else {
                    MeasureKind::Calculated
                };
                Measure {
                    ulid: definition.global_id().to_string(),
                    name: definition.name().to_owned(),
                    axes: definition.axes().iter().map(ToString::to_string).collect(),
                    kind: kind.into(),
                    ..Default::default()
                }
            })
            .collect();
        let dimensions = engine
            .dimensions()
            .iter()
            .map(|dimension| Dimension {
                ulid: dimension.global_id().to_string(),
                name: dimension.name().to_owned(),
                members: (0..dimension.len())
                    .filter_map(|id| dimension.member(MemberIndex(id)))
                    .map(|member| Member {
                        ulid: member.global_id().to_string(),
                        natural_key: member.natural_key().to_owned(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect();
        Response::ok(ListDefinitionsResponse {
            measures,
            dimensions,
            ..Default::default()
        })
    }

    async fn get_lag(
        &self,
        _context: RequestContext,
        _request: ServiceRequest<'_, GetLagRequest>,
    ) -> ServiceResult<GetLagResponse> {
        let lag = self.lag().map_err(loading_failure)?;
        Response::ok(GetLagResponse {
            committed: lag.committed.map(|sequence| sequence.0),
            recalculated: lag.recalculated.map(|sequence| sequence.0),
            ..Default::default()
        })
    }
}

// Not unavailable for a wrong process that is loading: the App server would wait for it as if it were the right one (ADR 0012).
#[connectrpc::async_trait]
impl Interceptor for Destination {
    async fn intercept_unary(
        &self,
        request: UnaryRequest,
        next: Next<'_>,
    ) -> Result<UnaryResponse, ConnectError> {
        let header = |name| {
            let mut values = request.ctx.headers().get_all(name).iter();
            match (values.next(), values.next()) {
                (Some(value), None) => value.to_str().ok(),
                _ => None,
            }
        };
        let workspace = header(WORKSPACE_HEADER).and_then(|text| text.parse().ok());
        let epoch = header(EPOCH_HEADER).and_then(|text| text.parse().ok().map(Epoch));
        if (workspace, epoch) != (Some(self.workspace), Some(self.epoch)) {
            return Err(ConnectError::failed_precondition(format!(
                "this assignee serves workspace {} at epoch {}",
                self.workspace, self.epoch.0
            )));
        }
        next.run(request).await
    }
}

fn edit_value(value: Option<&Value>) -> Result<Option<f64>, ConnectError> {
    match value {
        None => Err(ConnectError::invalid_argument("value is required")),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => value
            .as_number()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| ConnectError::invalid_argument("value must be a finite number or null")),
    }
}

fn measure(text: &str) -> Result<MeasureGlobalId, ConnectError> {
    global_id("measure", text)
}

// Not the members in the declared order: a client that read the definition before a change would name another cell (#184).
fn coordinate(members: Vec<AxisMember>) -> Result<Coordinate, ConnectError> {
    members
        .into_iter()
        .map(|member| {
            Ok((
                global_id("dimension", &member.dimension)?,
                global_id("member", &member.member)?,
            ))
        })
        .collect()
}

fn global_id<T: FromStr<Err = InvalidGlobalId>>(name: &str, text: &str) -> Result<T, ConnectError> {
    text.parse()
        .map_err(|error| ConnectError::invalid_argument(format!("{name} {text:?}: {error}")))
}

// Not a loaded flag in each answer: the App server records an Assignee only after a GetLag that is not unavailable (ADR 0012).
fn loading_failure(Loading: Loading) -> ConnectError {
    ConnectError::unavailable("the assignee is loading, retry later")
}

fn submit_failure(error: SubmitError) -> ConnectError {
    match error {
        SubmitError::Loading => loading_failure(Loading),
        SubmitError::Rejected(Rejection::Invalid(invalid)) => {
            ConnectError::invalid_argument(match invalid {
                Invalid::Undecodable(_) => "the edit cannot be decoded".to_owned(),
                Invalid::NotAnInput(measure) => format!("measure {measure} is not an input"),
                Invalid::Cell(error) => error.to_string(),
            })
        }
        SubmitError::Rejected(Rejection::Busy { behind }) => ConnectError::resource_exhausted(
            format!("the engine is {behind} commits behind, retry later"),
        ),
        SubmitError::Stopped => ConnectError::unavailable("the assignee stopped"),
    }
}

fn read_failure(error: ReadError) -> ConnectError {
    match error {
        ReadError::Loading => loading_failure(Loading),
        ReadError::NotCommitted(lag) => {
            let committed = match lag.committed {
                Some(sequence) => format!("the last Commit Sequence is {}", sequence.0),
                None => "the log has no commit".to_owned(),
            };
            ConnectError::unavailable(format!(
                "the sequence is not committed yet, retry later: {committed}"
            ))
        }
        ReadError::NoMeasure(measure) => ConnectError::not_found(format!("no measure {measure}")),
        ReadError::Cell(error) => ConnectError::invalid_argument(error.to_string()),
        ReadError::Stopped => ConnectError::unavailable("the assignee stopped"),
    }
}
