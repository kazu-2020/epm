mod coordinate;
mod support;

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use change_log::{AssigneeId, Epoch, ProcessId, Start};
use datagen::representative::{self, Size};
use futures_util::stream::BoxStream;
use measure::MeasureGlobalId;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use serde_json::{Value, json};
use server::http::{EPOCH_HEADER, Shutdown, WORKSPACE_HEADER};
use server::{Calculation, Engine, Exit, Handle, Settings};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use ulid::Ulid;
use workspace::{Workspace, WorkspaceGlobalId};

const QUANTITY: MeasureGlobalId = MeasureGlobalId(Ulid(100));
const SALES: MeasureGlobalId = MeasureGlobalId(Ulid(200));

struct App {
    handle: Handle,
    router: Router,
    coordinate: Vec<Value>,
    quantity: f64,
    destination: Vec<(&'static str, String)>,
}

async fn app(settings: Settings) -> App {
    let app = app_on(settings, Arc::new(InMemory::new()), 1);
    loaded(&app).await;
    app
}

async fn loaded(app: &App) {
    support::loaded(&app.handle).await;
}

fn app_on(settings: Settings, store: Arc<dyn ObjectStore>, epoch: u64) -> App {
    let engine = support::engine();
    let (coordinate, quantity) = quantity_cell(&engine);
    let (handle, assignee) = support::start(engine, Start::default(), settings, store, epoch);
    tokio::spawn(assignee);
    App {
        router: server::http::router(handle.clone()),
        handle,
        coordinate,
        quantity,
        destination: destination(&support::WORKSPACE.to_string(), &epoch.to_string()),
    }
}

fn quantity_cell(engine: &Engine) -> (Vec<Value>, f64) {
    let (coordinate, quantity) = engine
        .measures()
        .iter()
        .find(|measure| measure.global_id() == QUANTITY)
        .and_then(|quantity| quantity.cells().next())
        .expect("the seed has a quantity");
    let coordinate: Vec<Value> = coordinate::named(QUANTITY, &coordinate)
        .iter()
        .map(|(dimension, member)| {
            json!({ "dimension": dimension.to_string(), "member": member.to_string() })
        })
        .collect();
    (coordinate, quantity)
}

fn destination(workspace: &str, epoch: &str) -> Vec<(&'static str, String)> {
    vec![
        (WORKSPACE_HEADER, workspace.to_owned()),
        (EPOCH_HEADER, epoch.to_owned()),
    ]
}

async fn call(app: &App, method: &str, body: Value) -> (StatusCode, Value) {
    call_as(
        app,
        method,
        "application/json",
        &app.destination,
        body.to_string(),
    )
    .await
}

async fn call_as(
    app: &App,
    method: &str,
    content_type: &str,
    headers: &[(&str, String)],
    body: impl Into<Body>,
) -> (StatusCode, Value) {
    let mut request =
        Request::post(format!("/epm.v1.Workspace/{method}")).header(CONTENT_TYPE, content_type);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let request = request.body(body.into()).expect("a request");
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("a response");
    let status = response.status();
    let json = response
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|value| value == "application/json");
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let body = if json {
        serde_json::from_slice(&bytes).expect("a JSON body")
    } else {
        Value::Null
    };
    (status, body)
}

fn edit(
    request_id: &str,
    measure: MeasureGlobalId,
    coordinate: &[Value],
    value: Option<f64>,
) -> Value {
    json!({
        "request_id": request_id,
        "measure": measure.to_string(),
        "coordinate": coordinate,
        "value": value,
    })
}

fn cell(measure: MeasureGlobalId, coordinate: &[Value]) -> Value {
    json!({ "measure": measure.to_string(), "coordinate": coordinate })
}

async fn read(app: &App, at_least: Option<u64>) -> (StatusCode, Value) {
    let cells = [QUANTITY, SALES].map(|measure| cell(measure, &app.coordinate));
    call(
        app,
        "ReadCells",
        json!({ "at_least": at_least, "cells": cells }),
    )
    .await
}

async fn refusal(app: &App, method: &str, body: Value) -> (StatusCode, Value) {
    let (status, failure) = call(app, method, body).await;
    assert!(failure["message"].is_string(), "{failure}");
    (status, failure["code"].clone())
}

#[tokio::test]
async fn an_edit_changes_the_input_and_the_calculated_measure_that_reads_it() {
    let app = app(Settings::default()).await;
    let (status, before) = read(&app, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        before["recalculated"],
        Value::Null,
        "nothing is committed yet"
    );
    assert_eq!(before["values"][0], json!(app.quantity));

    let changed = app.quantity + 10.0;
    assert_eq!(
        call(
            &app,
            "SubmitEdit",
            edit("r1", QUANTITY, &app.coordinate, Some(changed))
        )
        .await,
        (StatusCode::OK, json!({ "sequence": "1" }))
    );
    let (status, edited) = read(&app, Some(1)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["recalculated"], json!("1"));
    assert_eq!(edited["values"][0], json!(changed));
    assert_ne!(
        edited["values"][1], before["values"][1],
        "the sales changed"
    );
}

#[tokio::test]
async fn a_null_value_clears_the_cell() {
    let app = app(Settings::default()).await;
    assert_eq!(
        call(
            &app,
            "SubmitEdit",
            edit("r1", QUANTITY, &app.coordinate, None)
        )
        .await,
        (StatusCode::OK, json!({ "sequence": "1" }))
    );
    let (_, cleared) = read(&app, Some(1)).await;
    assert_eq!(cleared["values"], json!([null, null]));
}

#[tokio::test]
async fn a_resend_of_a_request_id_gets_the_same_commit_sequence() {
    let app = app(Settings::default()).await;
    let body = edit("r1", QUANTITY, &app.coordinate, Some(1.0));
    let first = call(&app, "SubmitEdit", body.clone()).await;
    assert_eq!(first, (StatusCode::OK, json!({ "sequence": "1" })));
    assert_eq!(call(&app, "SubmitEdit", body).await, first);
    assert_eq!(
        call(&app, "GetLag", json!({})).await.1["committed"],
        json!("1"),
        "the resend took no Commit Sequence"
    );
}

#[tokio::test]
async fn an_edit_that_the_server_cannot_take_is_invalid_argument() {
    let app = app(Settings::default()).await;
    let (status, rejected) = call(
        &app,
        "SubmitEdit",
        edit("r1", SALES, &app.coordinate, Some(1.0)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        rejected,
        json!({
            "code": "invalid_argument",
            "message": format!("measure {SALES} is not an input"),
        })
    );

    let mut no_value = edit("r2", QUANTITY, &app.coordinate, Some(1.0));
    no_value.as_object_mut().expect("an object").remove("value");
    let mut bad_global_id = edit("r3", QUANTITY, &app.coordinate, Some(1.0));
    bad_global_id["measure"] = json!("not a ULID");
    let mut global_id_above_128_bits = edit("r7", QUANTITY, &app.coordinate, Some(1.0));
    global_id_above_128_bits["measure"] = json!(format!("8{}", &QUANTITY.to_string()[1..]));
    let mut unknown_field = edit("r4", QUANTITY, &app.coordinate, Some(1.0));
    unknown_field["note"] = json!("x");
    let mut unknown_field_in_coordinate = edit("r8", QUANTITY, &app.coordinate, Some(1.0));
    unknown_field_in_coordinate["coordinate"][0]["note"] = json!("x");
    let long_request_id = "r".repeat(129);
    let mut repeated_axis = app.coordinate.clone();
    repeated_axis.push(app.coordinate[0].clone());
    for body in [
        no_value,
        bad_global_id,
        global_id_above_128_bits,
        unknown_field,
        unknown_field_in_coordinate,
        edit("", QUANTITY, &app.coordinate, Some(1.0)),
        edit(&long_request_id, QUANTITY, &app.coordinate, Some(1.0)),
        edit("r5", QUANTITY, &app.coordinate[1..], Some(1.0)),
        edit("r9", QUANTITY, &repeated_axis, Some(1.0)),
    ] {
        assert_eq!(
            refusal(&app, "SubmitEdit", body.clone()).await,
            (StatusCode::BAD_REQUEST, json!("invalid_argument")),
            "{body}"
        );
    }
    let (status, failure) = call_as(
        &app,
        "SubmitEdit",
        "text/plain",
        &app.destination,
        edit("r6", QUANTITY, &app.coordinate, Some(1.0)).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{failure}");
    assert_eq!(
        call(&app, "GetLag", json!({})).await,
        (StatusCode::OK, json!({})),
        "no edit entered the log"
    );
}

#[tokio::test(start_paused = true)]
async fn each_refusal_has_its_connect_code() {
    let app = app(Settings {
        busy_after: 0,
        calculation: Calculation::Simulated(Duration::from_secs(10)),
        ..Settings::default()
    })
    .await;
    let unknown = MeasureGlobalId(Ulid(999));
    let read_of = |at_least: u64, cell: Value| json!({ "at_least": at_least, "cells": [cell] });
    assert_eq!(
        refusal(&app, "ReadCells", read_of(5, cell(unknown, &[]))).await,
        (StatusCode::NOT_FOUND, json!("not_found")),
        "not unavailable: no retry can read this measure"
    );
    assert_eq!(
        refusal(
            &app,
            "ReadCells",
            read_of(5, cell(SALES, &app.coordinate[1..]))
        )
        .await,
        (StatusCode::BAD_REQUEST, json!("invalid_argument"))
    );
    let mut outside = app.coordinate.clone();
    outside[0]["member"] = json!(Ulid(u128::MAX).to_string());
    assert_eq!(
        refusal(&app, "ReadCells", read_of(5, cell(SALES, &outside))).await,
        (StatusCode::BAD_REQUEST, json!("invalid_argument")),
        "not an empty cell: the member is not in the dimension"
    );
    assert_eq!(
        refusal(&app, "ReadCells", read_of(5, cell(SALES, &app.coordinate))).await,
        (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable"))
    );
    assert_eq!(
        refusal(
            &app,
            "ReadCells",
            json!({ "at_least": u64::MAX.to_string(), "cells": [cell(SALES, &app.coordinate)] })
        )
        .await,
        (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
        "the largest uint64 is a sequence that is not committed yet"
    );
    assert_eq!(
        refusal(&app, "ReadCells", read_of(0, cell(SALES, &app.coordinate))).await,
        (StatusCode::BAD_REQUEST, json!("invalid_argument")),
        "no commit has the sequence 0"
    );
    let (status, _) = call(
        &app,
        "SubmitEdit",
        edit("r1", QUANTITY, &app.coordinate, Some(1.0)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        refusal(
            &app,
            "SubmitEdit",
            edit("r2", QUANTITY, &app.coordinate, Some(2.0))
        )
        .await,
        (StatusCode::TOO_MANY_REQUESTS, json!("resource_exhausted")),
        "the engine is 1 commit behind"
    );
}

#[tokio::test]
async fn the_definitions_list_the_measures_and_the_members_of_their_dimensions() {
    let app = app(Settings::default()).await;
    let (status, definitions) = call(&app, "ListDefinitions", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let measures = definitions["measures"].as_array().expect("measures");
    let kind = |global_id: MeasureGlobalId| {
        measures
            .iter()
            .find(|measure| measure["ulid"] == global_id.to_string())
            .map(|measure| measure["kind"].clone())
    };
    assert_eq!(kind(QUANTITY), Some(json!("MEASURE_KIND_INPUT")));
    assert_eq!(kind(SALES), Some(json!("MEASURE_KIND_CALCULATED")));
    let first_axis = &measures[0]["axes"][0];
    let dimension = definitions["dimensions"]
        .as_array()
        .expect("dimensions")
        .iter()
        .find(|dimension| &dimension["ulid"] == first_axis)
        .expect("the axis is a listed dimension");
    assert!(dimension["members"][0]["ulid"].is_string());
    assert!(dimension["members"][0]["natural_key"].is_string());
}

#[tokio::test(start_paused = true)]
async fn every_call_is_unavailable_until_the_change_log_is_loaded() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let first = app_on(Settings::default(), Arc::clone(&store), 1);
    loaded(&first).await;
    assert_eq!(
        call(
            &first,
            "SubmitEdit",
            edit("r1", QUANTITY, &first.coordinate, None)
        )
        .await,
        (StatusCode::OK, json!({ "sequence": "1" }))
    );
    let config = ThrottleConfig {
        wait_list_per_call: Duration::from_secs(3),
        ..ThrottleConfig::default()
    };
    let second = app_on(
        Settings::default(),
        Arc::new(ThrottledStore::new(store, config)),
        2,
    );
    let cells = json!({ "cells": [cell(QUANTITY, &second.coordinate)] });
    for (method, body) in [
        ("GetLag", json!({})),
        ("ReadCells", cells.clone()),
        ("ListDefinitions", json!({})),
        ("SubmitEdit", edit("r2", QUANTITY, &second.coordinate, None)),
    ] {
        assert_eq!(
            refusal(&second, method, body).await,
            (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
            "{method} while the log has a commit that is not loaded"
        );
    }
    for (method, path, status) in [
        ("POST", "/epm.v1.Workspace/Unknown", StatusCode::NOT_FOUND),
        (
            "GET",
            "/epm.v1.Workspace/GetLag",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::empty())
            .expect("a request");
        let response = second
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("a response");
        assert_eq!(response.status(), status, "{method} {path} while loading");
    }
    loaded(&second).await;
    let (status, lag) = call(&second, "GetLag", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        lag["committed"],
        json!("1"),
        "the commit of the first assignee"
    );
}

#[derive(Debug)]
struct ThreadBlockingStore {
    inner: InMemory,
    released: Mutex<mpsc::Receiver<()>>,
}

impl fmt::Display for ThreadBlockingStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ThreadBlockingStore")
    }
}

#[async_trait]
impl ObjectStore for ThreadBlockingStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let _ = self
            .released
            .lock()
            .expect("no panic holds the lock")
            .recv();
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

fn launch() -> (Handle, impl Future<Output = Exit> + use<>, mpsc::Sender<()>) {
    let (release, released) = mpsc::channel();
    let (handle, assignee) = server::launch(
        Arc::new(ThreadBlockingStore {
            inner: InMemory::new(),
            released: Mutex::new(released),
        }),
        support::WORKSPACE,
        Workspace::try_new(representative::generate(&Size::TINY, 7)).expect("a valid workspace"),
        Settings::default(),
        AssigneeId {
            epoch: Epoch(1),
            process: ProcessId(1),
        },
    );
    (handle, assignee, release)
}

async fn post(address: SocketAddr, method: &str, body: &Value) -> (StatusCode, Value) {
    let body = body.to_string();
    let request = format!(
        "POST /epm.v1.Workspace/{method} HTTP/1.0\r\nhost: test\r\ncontent-type: application/json\r\n{WORKSPACE_HEADER}: {}\r\n{EPOCH_HEADER}: 1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        support::WORKSPACE,
        body.len()
    );
    let mut stream = TcpStream::connect(address)
        .await
        .expect("the server listens");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request is sent");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("a response");
    let (head, body) = response.split_once("\r\n\r\n").expect("a body");
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .expect("a status line");
    (status, serde_json::from_str(body).expect("a JSON body"))
}

#[tokio::test]
async fn the_server_listens_and_every_call_is_unavailable_while_the_snapshots_are_read() {
    let (handle, assignee, release) = launch();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(server::http::serve(
        listener,
        handle.clone(),
        assignee,
        std::future::pending(),
        Duration::from_secs(5),
    ));
    let (coordinate, _) = quantity_cell(&support::engine());
    let cells = json!({ "cells": [cell(QUANTITY, &coordinate)] });
    for (method, body) in [
        ("GetLag", json!({})),
        ("ReadCells", cells.clone()),
        ("ListDefinitions", json!({})),
        ("SubmitEdit", edit("r1", QUANTITY, &coordinate, Some(1.0))),
    ] {
        let (status, failure) = post(address, method, &body).await;
        assert_eq!(
            (status, failure["code"].clone()),
            (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
            "{method} while the snapshots are read"
        );
    }
    release.send(()).expect("the restore waits for the release");
    support::loaded(&handle).await;
    assert_eq!(
        post(
            address,
            "SubmitEdit",
            &edit("r1", QUANTITY, &coordinate, Some(1.0))
        )
        .await,
        (StatusCode::OK, json!({ "sequence": "1" }))
    );
}

#[tokio::test]
async fn a_shutdown_while_the_snapshots_are_read_stops_with_success_at_once() {
    let (handle, assignee, _restore_held) = launch();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let stopped = tokio::time::timeout(
        Duration::from_secs(5),
        server::http::serve(
            listener,
            handle,
            assignee,
            std::future::ready(()),
            Duration::from_secs(30),
        ),
    )
    .await
    .expect("serve does not wait for the snapshot read");
    assert!(matches!(stopped, Shutdown::Drained), "{stopped:?}");
}

#[tokio::test]
async fn a_coordinate_names_its_axes_in_any_order() {
    let app = app(Settings::default()).await;
    let mut reversed = app.coordinate.clone();
    reversed.reverse();
    let (status, read) = call(
        &app,
        "ReadCells",
        json!({ "cells": [cell(QUANTITY, &reversed)] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["values"][0], json!(app.quantity));
}

#[tokio::test]
async fn a_misspelled_field_is_invalid_argument_and_not_a_read_without_freshness() {
    let app = app(Settings::default()).await;
    let cells = [cell(QUANTITY, &app.coordinate)];
    for (method, body) in [
        ("ReadCells", json!({ "at_lest": 0, "cells": cells })),
        ("GetLag", json!({ "note": "x" })),
        ("ListDefinitions", json!({ "note": "x" })),
    ] {
        assert_eq!(
            refusal(&app, method, body).await,
            (StatusCode::BAD_REQUEST, json!("invalid_argument")),
            "{method}"
        );
    }
}

fn length_delimited(field: u8, bytes: &[u8]) -> Vec<u8> {
    let length = u8::try_from(bytes.len()).expect("a field shorter than 128 bytes");
    assert!(length < 0x80, "the length fits in one varint byte");
    [&[field << 3 | 2, length][..], bytes].concat()
}

fn binary_edit(app: &App, value: &[u8]) -> Vec<u8> {
    let mut edit = [
        length_delimited(1, b"r1"),
        length_delimited(2, QUANTITY.to_string().as_bytes()),
    ]
    .concat();
    for axis in &app.coordinate {
        let member = [
            length_delimited(1, axis["dimension"].as_str().expect("a ULID").as_bytes()),
            length_delimited(2, axis["member"].as_str().expect("a ULID").as_bytes()),
        ]
        .concat();
        edit.extend(length_delimited(3, &member));
    }
    edit.extend(length_delimited(4, value));
    edit
}

#[tokio::test]
async fn a_binary_value_that_json_cannot_hold_is_invalid_argument_and_clears_nothing() {
    let app = app(Settings::default()).await;
    let number_value = |number: f64| [&[2 << 3 | 1][..], &number.to_le_bytes()].concat();
    for (case, value) in [
        ("a value with no kind", Vec::new()),
        ("NaN", number_value(f64::NAN)),
        ("infinity", number_value(f64::INFINITY)),
    ] {
        let (status, failure) = call_as(
            &app,
            "SubmitEdit",
            "application/proto",
            &app.destination,
            binary_edit(&app, &value),
        )
        .await;
        assert_eq!(
            (status, failure["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("invalid_argument")),
            "{case}"
        );
    }
    let (status, _) = call_as(
        &app,
        "SubmitEdit",
        "application/proto",
        &app.destination,
        binary_edit(&app, &number_value(1.0)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the hand-made bytes are a valid edit"
    );
    let (_, read) = read(&app, Some(1)).await;
    assert_eq!(
        read["values"][0],
        json!(1.0),
        "only the valid edit committed"
    );
}

fn wrong_destinations() -> [(&'static str, Vec<(&'static str, String)>); 8] {
    let workspace = support::WORKSPACE.to_string();
    let other_workspace = WorkspaceGlobalId(Ulid(2)).to_string();
    [
        ("no workspace", vec![(EPOCH_HEADER, "1".to_owned())]),
        ("no epoch", vec![(WORKSPACE_HEADER, workspace.clone())]),
        ("other workspace", destination(&other_workspace, "1")),
        ("older epoch", destination(&workspace, "0")),
        ("workspace not a ULID", destination("workspace~", "1")),
        ("epoch not a number", destination(&workspace, "one")),
        (
            "workspace twice",
            [
                destination(&workspace, "1"),
                vec![(WORKSPACE_HEADER, other_workspace.clone())],
            ]
            .concat(),
        ),
        (
            "epoch twice",
            [
                destination(&workspace, "1"),
                vec![(EPOCH_HEADER, "0".to_owned())],
            ]
            .concat(),
        ),
    ]
}

fn every_operation(app: &App) -> [(&'static str, Value); 4] {
    [
        (
            "SubmitEdit",
            edit("r1", QUANTITY, &app.coordinate, Some(1.0)),
        ),
        (
            "ReadCells",
            json!({ "cells": [cell(QUANTITY, &app.coordinate)] }),
        ),
        ("ListDefinitions", json!({})),
        ("GetLag", json!({})),
    ]
}

#[tokio::test]
async fn a_call_for_another_workspace_or_epoch_is_failed_precondition_and_commits_nothing() {
    let app = app(Settings::default()).await;
    for (method, body) in every_operation(&app) {
        for (case, headers) in wrong_destinations() {
            let (status, failure) =
                call_as(&app, method, "application/json", &headers, body.to_string()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method} with {case}");
            assert_eq!(
                failure,
                json!({
                    "code": "failed_precondition",
                    "message": format!("this assignee serves workspace {} at epoch 1", support::WORKSPACE),
                }),
                "{method} with {case}"
            );
        }
    }
    assert_eq!(
        call(&app, "GetLag", json!({})).await,
        (StatusCode::OK, json!({})),
        "no edit entered the log"
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_for_another_epoch_is_failed_precondition_while_the_change_log_is_loading() {
    let config = ThrottleConfig {
        wait_list_per_call: Duration::from_secs(3),
        ..ThrottleConfig::default()
    };
    let store = ThrottledStore::new(InMemory::new(), config);
    let second = app_on(Settings::default(), Arc::new(store), 2);
    for (method, body) in every_operation(&second) {
        let (status, failure) = call_as(
            &second,
            method,
            "application/json",
            &destination(&support::WORKSPACE.to_string(), "1"),
            body.to_string(),
        )
        .await;
        assert_eq!(
            (status, failure["code"].clone()),
            (StatusCode::BAD_REQUEST, json!("failed_precondition")),
            "{method} from the process of epoch 1 is not taken as loading"
        );
    }
    assert_eq!(
        refusal(&second, "GetLag", json!({})).await,
        (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
        "the change log is still loading"
    );
}
