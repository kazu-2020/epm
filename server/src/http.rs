use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use change_log::{RequestId, Sequence};
use dimension::{InvalidGlobalId, MemberIndex};
use measure::MeasureGlobalId;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use workspace::Coordinate;

use crate::{Edit, Exit, Handle, Invalid, Loading, ReadError, Rejection, SubmitError};

// Not only the body limit: the change log keeps every Request ID, and each new assignee reads them all again.
const MAXIMUM_REQUEST_ID_BYTES: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitEditRequest {
    request_id: String,
    measure: String,
    coordinate: Vec<AxisMember>,
    // Not the default for a missing Option: a request that forgets the value would clear the cell.
    #[serde(deserialize_with = "Option::deserialize")]
    value: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCellsRequest {
    at_least: Option<u64>,
    cells: Vec<CellRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellRequest {
    measure: String,
    coordinate: Vec<AxisMember>,
}

// Not the members in the declared order: a client that read the definition before a change would name another cell (#184).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AxisMember {
    dimension: String,
    member: String,
}

#[derive(Clone, Copy)]
enum Code {
    InvalidArgument,
    NotFound,
    ResourceExhausted,
    Unavailable,
}

struct Failure {
    code: Code,
    message: String,
}

#[derive(Debug)]
pub enum Shutdown {
    Drained,
    Deadline,
    Assignee(Exit),
}

pub fn router(handle: Handle) -> Router {
    Router::new()
        .route("/epm.v1.Workspace/SubmitEdit", post(submit_edit))
        .route("/epm.v1.Workspace/ReadCells", post(read_cells))
        .route("/epm.v1.Workspace/ListDefinitions", post(list_definitions))
        .route("/epm.v1.Workspace/GetLag", post(get_lag))
        .with_state(handle)
}

pub async fn serve(
    listener: TcpListener,
    handle: Handle,
    assignee: impl Future<Output = Exit>,
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
    tokio::select! {
        exit = assignee => Shutdown::Assignee(exit),
        served = axum::serve(listener, router(handle)).with_graceful_shutdown(shutdown) => {
            served.expect("the graceful serve of axum 0.8 returns only Ok");
            Shutdown::Drained
        }
        () = deadline_passed => Shutdown::Deadline,
    }
}

async fn submit_edit(
    State(handle): State<Handle>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Failure> {
    let request: SubmitEditRequest = parse(&headers, &body)?;
    if request.request_id.is_empty() || request.request_id.len() > MAXIMUM_REQUEST_ID_BYTES {
        return Err(Failure::new(
            Code::InvalidArgument,
            format!("request_id must have 1 to {MAXIMUM_REQUEST_ID_BYTES} bytes"),
        ));
    }
    let edit = Edit {
        measure: measure(&request.measure)?,
        coordinate: coordinate(request.coordinate)?,
        value: request.value,
    };
    let request_id = RequestId(Bytes::from(request.request_id));
    let sequence = handle
        .submit(request_id, &edit)
        .await
        .map_err(submit_failure)?;
    Ok(Json(json!({ "sequence": sequence.0 })))
}

async fn read_cells(
    State(handle): State<Handle>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Failure> {
    let request: ReadCellsRequest = parse(&headers, &body)?;
    let cells = request
        .cells
        .into_iter()
        .map(|cell| Ok((measure(&cell.measure)?, coordinate(cell.coordinate)?)))
        .collect::<Result<Vec<_>, Failure>>()?;
    let (recalculated, values) = handle
        .read(request.at_least.map(Sequence), &cells)
        .await
        .map_err(read_failure)?;
    Ok(Json(json!({
        "recalculated": recalculated.map(|sequence| sequence.0),
        "values": values,
    })))
}

async fn list_definitions(State(handle): State<Handle>) -> Result<Json<Value>, Failure> {
    let engine = handle.definitions().map_err(loading_failure)?;
    let measures: Vec<Value> = engine
        .measures()
        .iter()
        .map(|measure| {
            let definition = measure.definition();
            let kind = if engine.edit_check().is_input(definition.global_id()) {
                "input"
            } else {
                "calculated"
            };
            json!({
                "ulid": definition.global_id().to_string(),
                "name": definition.name(),
                "axes": definition.axes().iter().map(ToString::to_string).collect::<Vec<_>>(),
                "kind": kind,
            })
        })
        .collect();
    let dimensions: Vec<Value> = engine
        .dimensions()
        .iter()
        .map(|dimension| {
            let members: Vec<Value> = (0..dimension.len())
                .filter_map(|id| dimension.member(MemberIndex(id)))
                .map(|member| {
                    json!({ "ulid": member.global_id().to_string(), "natural_key": member.natural_key() })
                })
                .collect();
            json!({
                "ulid": dimension.global_id().to_string(),
                "name": dimension.name(),
                "members": members,
            })
        })
        .collect();
    Ok(Json(
        json!({ "measures": measures, "dimensions": dimensions }),
    ))
}

async fn get_lag(State(handle): State<Handle>) -> Result<Json<Value>, Failure> {
    let lag = handle.lag().map_err(loading_failure)?;
    Ok(Json(json!({
        "committed": lag.committed.map(|sequence| sequence.0),
        "recalculated": lag.recalculated.map(|sequence| sequence.0),
    })))
}

// Not the Json extractor of axum: its rejection is plain text, not the error body of this API.
fn parse<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, Failure> {
    // Not any content type: a web page can send text/plain to loopback without a CORS preflight, and the edit would commit.
    let json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(Failure::new(
            Code::InvalidArgument,
            "content-type must be application/json".to_owned(),
        ));
    }
    serde_json::from_slice(body)
        .map_err(|error| Failure::new(Code::InvalidArgument, error.to_string()))
}

fn measure(text: &str) -> Result<MeasureGlobalId, Failure> {
    global_id("measure", text)
}

fn coordinate(members: Vec<AxisMember>) -> Result<Coordinate, Failure> {
    members
        .into_iter()
        .map(|AxisMember { dimension, member }| {
            Ok((
                global_id("dimension", &dimension)?,
                global_id("member", &member)?,
            ))
        })
        .collect()
}

fn global_id<T: FromStr<Err = InvalidGlobalId>>(name: &str, text: &str) -> Result<T, Failure> {
    text.parse()
        .map_err(|error| Failure::new(Code::InvalidArgument, format!("{name} {text:?}: {error}")))
}

// Not a loaded flag in each answer: the App server records an Assignee only after a GetLag that is not unavailable (ADR 0012).
fn loading_failure(Loading: Loading) -> Failure {
    Failure::new(
        Code::Unavailable,
        "the assignee is loading the change log, retry later".to_owned(),
    )
}

fn submit_failure(error: SubmitError) -> Failure {
    match error {
        SubmitError::Loading => loading_failure(Loading),
        SubmitError::Rejected(Rejection::Invalid(invalid)) => {
            let message = match invalid {
                Invalid::Undecodable(_) => "the edit cannot be decoded".to_owned(),
                Invalid::NotAnInput(measure) => format!("measure {measure} is not an input"),
                Invalid::Cell(error) => error.to_string(),
            };
            Failure::new(Code::InvalidArgument, message)
        }
        SubmitError::Rejected(Rejection::Busy { behind }) => Failure::new(
            Code::ResourceExhausted,
            format!("the engine is {behind} commits behind, retry later"),
        ),
        SubmitError::Stopped => Failure::new(Code::Unavailable, "the assignee stopped".to_owned()),
    }
}

fn read_failure(error: ReadError) -> Failure {
    match error {
        ReadError::Loading => loading_failure(Loading),
        ReadError::NotCommitted(lag) => {
            let committed = match lag.committed {
                Some(sequence) => format!("the last Commit Sequence is {}", sequence.0),
                None => "the log has no commit".to_owned(),
            };
            Failure::new(
                Code::Unavailable,
                format!("the sequence is not committed yet, retry later: {committed}"),
            )
        }
        ReadError::NoMeasure(measure) => {
            Failure::new(Code::NotFound, format!("no measure {measure}"))
        }
        ReadError::Cell(error) => Failure::new(Code::InvalidArgument, error.to_string()),
        ReadError::Stopped => Failure::new(Code::Unavailable, "the assignee stopped".to_owned()),
    }
}

impl Code {
    fn connect(self) -> (&'static str, StatusCode) {
        match self {
            Code::InvalidArgument => ("invalid_argument", StatusCode::BAD_REQUEST),
            Code::NotFound => ("not_found", StatusCode::NOT_FOUND),
            Code::ResourceExhausted => ("resource_exhausted", StatusCode::TOO_MANY_REQUESTS),
            Code::Unavailable => ("unavailable", StatusCode::SERVICE_UNAVAILABLE),
        }
    }
}

impl Failure {
    fn new(code: Code, message: String) -> Failure {
        Failure { code, message }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (code, status) = self.code.connect();
        let body = json!({ "code": code, "message": self.message });
        (status, Json(body)).into_response()
    }
}
