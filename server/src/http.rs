use std::sync::Arc;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use change_log::{RequestId, Sequence};
use dimension::MemberIndex;
use measure::{MeasureError, MeasureGlobalId};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use ulid::Ulid;

use crate::{Edit, Handle, Invalid, ReadError, Rejection, SubmitError};

// Not only the body limit: the change log keeps every Request ID, and each new owner reads them all again.
const MAXIMUM_REQUEST_ID_BYTES: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitEditRequest {
    request_id: String,
    measure: String,
    coordinate: Vec<u32>,
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
    coordinate: Vec<u32>,
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

pub fn router(handle: Handle) -> Router {
    Router::new()
        .route("/epm.v1.Workspace/SubmitEdit", post(submit_edit))
        .route("/epm.v1.Workspace/ReadCells", post(read_cells))
        .route("/epm.v1.Workspace/ListDefinitions", post(list_definitions))
        .route("/epm.v1.Workspace/GetLag", post(get_lag))
        .with_state(handle)
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
        coordinate: coordinate(request.coordinate),
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
        .map(|cell| Ok((measure(&cell.measure)?, coordinate(cell.coordinate))))
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

async fn list_definitions(State(handle): State<Handle>) -> Json<Value> {
    let engine = Arc::clone(&handle.published().borrow());
    let measures: Vec<Value> = engine
        .measures()
        .iter()
        .map(|measure| {
            let definition = measure.definition();
            let kind = if engine.definitions().is_input(definition.ulid) {
                "input"
            } else {
                "calculated"
            };
            json!({
                "ulid": definition.ulid.0.to_string(),
                "name": definition.name,
                "axes": definition.axes.iter().map(|axis| axis.0.to_string()).collect::<Vec<_>>(),
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
                .map(|member| json!({ "id": member.id().0, "key": member.key(), "name": member.name() }))
                .collect();
            json!({
                "ulid": dimension.ulid().0.to_string(),
                "name": dimension.name(),
                "members": members,
            })
        })
        .collect();
    Json(json!({ "measures": measures, "dimensions": dimensions }))
}

async fn get_lag(State(handle): State<Handle>) -> Json<Value> {
    let lag = handle.lag();
    Json(json!({
        "committed": lag.committed.map(|sequence| sequence.0),
        "recalculated": lag.recalculated.map(|sequence| sequence.0),
    }))
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
    Ulid::from_string(text)
        .map(MeasureGlobalId)
        .map_err(|error| Failure::new(Code::InvalidArgument, format!("measure {text:?}: {error}")))
}

fn coordinate(members: Vec<u32>) -> Vec<MemberIndex> {
    members.into_iter().map(MemberIndex).collect()
}

fn submit_failure(error: SubmitError) -> Failure {
    match error {
        SubmitError::Rejected(Rejection::Invalid(invalid)) => {
            let message = match invalid {
                Invalid::Undecodable(_) => "the edit cannot be decoded".to_owned(),
                Invalid::NotAnInput(measure) => format!("measure {} is not an input", measure.0),
                Invalid::Coordinate(error) => coordinate_message(&error),
            };
            Failure::new(Code::InvalidArgument, message)
        }
        SubmitError::Rejected(Rejection::Busy { behind }) => Failure::new(
            Code::ResourceExhausted,
            format!("the engine is {behind} commits behind, retry later"),
        ),
        SubmitError::Stopped => Failure::new(Code::Unavailable, "the owner stopped".to_owned()),
    }
}

fn read_failure(error: ReadError) -> Failure {
    match error {
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
            Failure::new(Code::NotFound, format!("no measure {}", measure.0))
        }
        ReadError::Coordinate(error) => {
            Failure::new(Code::InvalidArgument, coordinate_message(&error))
        }
        ReadError::Stopped => Failure::new(Code::Unavailable, "the owner stopped".to_owned()),
    }
}

fn coordinate_message(error: &MeasureError) -> String {
    match error {
        MeasureError::CoordinateLength { expected, found } => {
            format!("the coordinate needs {expected} members, but it has {found}")
        }
        MeasureError::MemberOutOfRange { axis, member } => {
            format!("member {} is not in dimension {}", member.0, axis.0)
        }
        other => other.to_string(),
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
