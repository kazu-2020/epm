use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use app_server_prototype::proto::epm::v1::{
    AxisMember, Cell, GetLagRequest, ListDefinitionsRequest, ReadCellsRequest, SubmitEditRequest,
};
use app_server_prototype::{
    Assignee, EPOCH_HEADER, EditValue, EpochCheck, FIRST_SEQUENCE, WORKSPACE_HEADER,
    assignee_client, router, spawn,
};
use buffa::MessageField;
use buffa_types::google::protobuf::Value;
use connectrpc::ErrorCode;
use connectrpc::client::CallOptions;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const WORKSPACE: &str = "01JCQ7ZK3M5V8W9X0Y1Z2A3B4C";
const EPOCH: u64 = 7;
const MEASURE: &str = "00000000000000000000000034";
const JSON: Option<&str> = Some("application/json");
const DIMENSION: &str = "00000000000000000000000001";
const MEMBER: &str = "00000000000000000000000002";
const COORDINATE: &str =
    r#"[{"dimension":"00000000000000000000000001","member":"00000000000000000000000002"}]"#;

fn coordinate() -> Vec<AxisMember> {
    vec![AxisMember {
        dimension: DIMENSION.to_owned(),
        member: MEMBER.to_owned(),
        ..Default::default()
    }]
}

async fn start(assignee: Assignee) -> (SocketAddr, Arc<Assignee>) {
    let assignee = Arc::new(assignee);
    let check = EpochCheck {
        workspace: WORKSPACE.to_owned(),
        epoch: EPOCH,
    };
    (spawn(router(assignee.clone(), check)).await, assignee)
}

fn edit(request_id: &str, value: Option<Value>) -> SubmitEditRequest {
    let mut request = SubmitEditRequest {
        request_id: request_id.to_owned(),
        measure: MEASURE.to_owned(),
        coordinate: coordinate(),
        ..Default::default()
    };
    if let Some(value) = value {
        request.value = MessageField::some(value);
    }
    request
}

// Not the generated client: these requests must be the bytes that curl sends today.
// Not HTTP/1.1: the Connect response body is chunked, and this reader does not decode chunks.
async fn post(
    address: SocketAddr,
    method: &str,
    content_type: Option<&str>,
    headers: &[(&str, String)],
    body: &str,
) -> (u16, String) {
    let mut request = format!(
        "POST /epm.v1.Workspace/{method} HTTP/1.0\r\nhost: {address}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    if let Some(content_type) = content_type {
        request.push_str(&format!("content-type: {content_type}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let mut stream = TcpStream::connect(address).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("http response");
    assert!(
        !head.to_ascii_lowercase().contains("transfer-encoding"),
        "{head}"
    );
    let status = head[9..12].parse().expect("status code");
    (status, body.to_owned())
}

fn identity() -> Vec<(&'static str, String)> {
    vec![
        (WORKSPACE_HEADER, WORKSPACE.to_owned()),
        (EPOCH_HEADER, EPOCH.to_string()),
    ]
}

async fn post_json(address: SocketAddr, method: &str, body: &str) -> (u16, serde_json::Value) {
    let (status, body) = post(address, method, JSON, &identity(), body).await;
    (status, serde_json::from_str(&body).expect("json body"))
}

#[tokio::test]
async fn four_operations_round_trip_through_the_generated_client() {
    let (address, _) = start(Assignee::new(None, Duration::ZERO)).await;
    let client = assignee_client(address, WORKSPACE, EPOCH);

    let submitted = client
        .submit_edit(edit("r1", Some(Value::from(3.0))))
        .await
        .expect("submit edit")
        .into_owned();
    let read = client
        .read_cells(ReadCellsRequest {
            at_least: Some(submitted.sequence),
            cells: vec![Cell {
                measure: MEASURE.to_owned(),
                coordinate: coordinate(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .expect("read cells")
        .into_owned();
    let definitions = client
        .list_definitions(ListDefinitionsRequest::default())
        .await
        .expect("list definitions")
        .into_owned();
    let lag = client
        .get_lag(GetLagRequest::default())
        .await
        .expect("get lag")
        .into_owned();

    assert_eq!(submitted.sequence, FIRST_SEQUENCE);
    assert_eq!(read.recalculated, Some(FIRST_SEQUENCE));
    assert_eq!(read.values, vec![Value::from(1.5)]);
    assert_eq!(definitions.measures[0].ulid, MEASURE);
    assert_eq!(lag.committed, Some(FIRST_SEQUENCE));
    assert_eq!(lag.recalculated, None);
}

#[tokio::test]
async fn existing_json_requests_are_accepted_and_responses_differ_only_as_listed() {
    let (address, _) = start(Assignee::new(None, Duration::ZERO)).await;

    let submitted = post_json(
        address,
        "SubmitEdit",
        &format!(r#"{{"request_id":"r1","measure":"00000000000000000000000034","coordinate":{COORDINATE},"value":3}}"#),
    )
    .await;
    let read = post_json(
        address,
        "ReadCells",
        &format!(r#"{{"at_least":0,"cells":[{{"measure":"00000000000000000000000068","coordinate":{COORDINATE}}},{{"measure":"00000000000000000000000068","coordinate":{COORDINATE}}}]}}"#),
    )
    .await;
    let lag = post_json(address, "GetLag", "{}").await;
    let definitions = post_json(address, "ListDefinitions", "{}").await;
    let unknown_field = post_json(
        address,
        "SubmitEdit",
        &format!(r#"{{"request_id":"r2","measure":"00000000000000000000000034","coordinate":{COORDINATE},"value":3,"comment":"x"}}"#),
    )
    .await;
    let form_content_type = post(
        address,
        "GetLag",
        Some("application/x-www-form-urlencoded"),
        &identity(),
        "{}",
    )
    .await;

    assert_eq!(submitted, (200, json!({ "sequence": "9007199254740993" })));
    assert_eq!(
        read,
        (200, json!({ "recalculated": "0", "values": [1.5, null] }))
    );
    assert_eq!(lag, (200, json!({ "committed": "9007199254740993" })));
    assert_eq!(
        definitions,
        (
            200,
            json!({ "measures": [{
                "ulid": MEASURE,
                "name": "Units",
                "axes": ["00000000000000000000000001"],
                "kind": "input",
            }] })
        )
    );
    assert_eq!(unknown_field.0, 200, "{unknown_field:?}");
    assert_eq!(form_content_type.0, 415, "{form_content_type:?}");
}

#[tokio::test]
async fn null_value_clears_the_cell_and_a_missing_value_is_rejected() {
    let (address, assignee) = start(Assignee::new(None, Duration::ZERO)).await;
    let client = assignee_client(address, WORKSPACE, EPOCH);

    let json_null = post_json(
        address,
        "SubmitEdit",
        &format!(r#"{{"request_id":"json-null","measure":"00000000000000000000000034","coordinate":{COORDINATE},"value":null}}"#),
    )
    .await;
    let json_missing = post_json(
        address,
        "SubmitEdit",
        &format!(r#"{{"request_id":"json-missing","measure":"00000000000000000000000034","coordinate":{COORDINATE}}}"#),
    )
    .await;
    let json_string = post_json(
        address,
        "SubmitEdit",
        &format!(r#"{{"request_id":"json-string","measure":"00000000000000000000000034","coordinate":{COORDINATE},"value":"3"}}"#),
    )
    .await;
    let binary_null = client
        .submit_edit(edit("binary-null", Some(Value::null())))
        .await;
    let binary_missing = client.submit_edit(edit("binary-missing", None)).await;

    assert_eq!(json_null.0, 200, "{json_null:?}");
    assert_eq!(assignee.accepted_value("json-null"), Some(EditValue::Clear));
    assert_eq!(json_missing.0, 400, "{json_missing:?}");
    assert_eq!(json_missing.1["code"], "invalid_argument");
    assert_eq!(json_string.1["code"], "invalid_argument");
    assert!(binary_null.is_ok());
    assert_eq!(
        assignee.accepted_value("binary-null"),
        Some(EditValue::Clear)
    );
    let binary_missing = binary_missing.expect_err("missing value is rejected");
    assert_eq!(binary_missing.code, ErrorCode::InvalidArgument);
    assert_eq!(assignee.accepted_value("binary-missing"), None);
}

#[tokio::test]
async fn sixty_four_bit_values_survive_json_numbers_json_strings_and_binary() {
    let (address, _) = start(Assignee::new(None, Duration::ZERO)).await;
    let client = assignee_client(address, WORKSPACE, EPOCH);
    let cells = format!(
        r#""cells":[{{"measure":"00000000000000000000000068","coordinate":{COORDINATE}}}]"#
    );

    let json_number = post_json(
        address,
        "ReadCells",
        &format!(r#"{{"at_least":18446744073709551615,{cells}}}"#),
    )
    .await;
    let json_string = post_json(
        address,
        "ReadCells",
        &format!(r#"{{"at_least":"18446744073709551615",{cells}}}"#),
    )
    .await;
    let binary = client
        .read_cells(ReadCellsRequest {
            at_least: Some(u64::MAX),
            ..Default::default()
        })
        .await
        .expect("read cells")
        .into_owned();

    assert_eq!(json_number.1["recalculated"], "18446744073709551615");
    assert_eq!(json_string.1["recalculated"], "18446744073709551615");
    assert_eq!(binary.recalculated, Some(u64::MAX));
}

#[tokio::test]
async fn error_codes_are_distinguishable_by_the_client() {
    let codes = [
        ErrorCode::Unavailable,
        ErrorCode::FailedPrecondition,
        ErrorCode::InvalidArgument,
        ErrorCode::NotFound,
        ErrorCode::ResourceExhausted,
    ];
    for code in codes {
        let (address, _) = start(Assignee::new(Some(code), Duration::ZERO)).await;
        let client = assignee_client(address, WORKSPACE, EPOCH);

        let error = client
            .get_lag(GetLagRequest::default())
            .await
            .expect_err("configured failure");
        let (status, body) = post_json(address, "GetLag", "{}").await;

        assert_eq!(error.code, code);
        assert_eq!(status, code.http_status().as_u16());
        assert_eq!(body["code"], code.as_str());
    }
}

#[tokio::test]
async fn mismatched_workspace_or_epoch_header_gets_failed_precondition() {
    let (address, _) = start(Assignee::new(None, Duration::ZERO)).await;
    let stale_epoch = assignee_client(address, WORKSPACE, EPOCH - 1);
    let other_workspace = assignee_client(address, "01JCQ7ZK3M5V8W9X0Y1Z2A3B4D", EPOCH);

    let stale = stale_epoch.get_lag(GetLagRequest::default()).await;
    let other = other_workspace
        .submit_edit(edit("r1", Some(Value::from(1.0))))
        .await;
    let without_headers = post(address, "GetLag", JSON, &[], "{}").await;

    assert_eq!(
        stale.expect_err("stale epoch").code,
        ErrorCode::FailedPrecondition
    );
    assert_eq!(
        other.expect_err("other workspace").code,
        ErrorCode::FailedPrecondition
    );
    assert_eq!(without_headers.0, 400, "{without_headers:?}");
    assert!(
        without_headers.1.contains("failed_precondition"),
        "{without_headers:?}"
    );
}

#[tokio::test]
async fn client_timeout_reaches_the_server_and_a_retry_with_the_same_request_id_gets_the_same_sequence()
 {
    let (address, assignee) = start(Assignee::new(None, Duration::from_secs(5))).await;
    let client = assignee_client(address, WORKSPACE, EPOCH);
    let request = edit("unknown-result", Some(Value::from(2.0)));
    let short = CallOptions::default().with_timeout(Duration::from_millis(200));

    let first = client
        .submit_edit_with_options(request.clone(), short.clone())
        .await;
    let retry = client
        .submit_edit_with_options(request, short)
        .await
        .expect("retry")
        .into_owned();

    assert_eq!(
        first.expect_err("timeout").code,
        ErrorCode::DeadlineExceeded
    );
    let remaining = assignee.observed_time_remaining()[0].expect("server sees the deadline");
    assert!(remaining <= Duration::from_millis(200), "{remaining:?}");
    assert_eq!(retry.sequence, FIRST_SEQUENCE);
    assert_eq!(assignee.accepted_edits(), 1);
}

#[tokio::test]
async fn connect_service_shares_one_axum_router_and_listener_with_plain_routes() {
    let (address, _) = start(Assignee::new(None, Duration::ZERO)).await;
    let mut stream = TcpStream::connect(address).await.expect("connect");
    stream
        .write_all(b"GET /health HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut health = String::new();
    stream.read_to_string(&mut health).await.expect("read");

    let lag = assignee_client(address, WORKSPACE, EPOCH)
        .get_lag(GetLagRequest::default())
        .await;

    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.ends_with("ok"), "{health}");
    assert!(lag.is_ok());
}
