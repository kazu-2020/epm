use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use change_log::Epoch;
use datagen::representative::{self, Size};
use measure::MeasureUlid;
use object_store::memory::InMemory;
use serde_json::{Value, json};
use server::{Calculation, Engine, Owner, Settings};
use tower::ServiceExt;
use ulid::Ulid;

const QUANTITY: MeasureUlid = MeasureUlid(Ulid(100));
const SALES: MeasureUlid = MeasureUlid(Ulid(200));

struct App {
    router: Router,
    coordinate: Vec<u32>,
    quantity: f64,
}

fn app(settings: Settings) -> App {
    let engine = Engine::new(representative::generate(&Size::TINY, 7)).expect("valid formulas");
    let (coordinate, quantity) = engine
        .measures()
        .iter()
        .find(|measure| measure.ulid() == QUANTITY)
        .and_then(|quantity| quantity.cells().next())
        .expect("the seed has a quantity");
    let (owner, handle) = Owner::new(engine, settings);
    tokio::spawn(owner.run(Arc::new(InMemory::new()), "ws", Epoch(1)));
    App {
        router: server::http::router(handle),
        coordinate: coordinate.iter().map(|member| member.0).collect(),
        quantity,
    }
}

async fn call(app: &App, method: &str, body: Value) -> (StatusCode, Value) {
    call_as(app, method, "application/json", body).await
}

async fn call_as(app: &App, method: &str, content_type: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::post(format!("/epm.v1.Workspace/{method}"))
        .header(CONTENT_TYPE, content_type)
        .body(Body::from(body.to_string()))
        .expect("a request");
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("a response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (status, serde_json::from_slice(&bytes).expect("a JSON body"))
}

fn edit(request_id: &str, measure: MeasureUlid, coordinate: &[u32], value: Option<f64>) -> Value {
    json!({
        "request_id": request_id,
        "measure": measure.0.to_string(),
        "coordinate": coordinate,
        "value": value,
    })
}

fn cell(measure: MeasureUlid, coordinate: &[u32]) -> Value {
    json!({ "measure": measure.0.to_string(), "coordinate": coordinate })
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
    let app = app(Settings::default());
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
        (StatusCode::OK, json!({ "sequence": 0 }))
    );
    let (status, edited) = read(&app, Some(0)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["recalculated"], json!(0));
    assert_eq!(edited["values"][0], json!(changed));
    assert_ne!(
        edited["values"][1], before["values"][1],
        "the sales changed"
    );
}

#[tokio::test]
async fn a_null_value_clears_the_cell() {
    let app = app(Settings::default());
    assert_eq!(
        call(
            &app,
            "SubmitEdit",
            edit("r1", QUANTITY, &app.coordinate, None)
        )
        .await,
        (StatusCode::OK, json!({ "sequence": 0 }))
    );
    let (_, cleared) = read(&app, Some(0)).await;
    assert_eq!(cleared["values"], json!([null, null]));
}

#[tokio::test]
async fn a_resend_of_a_request_id_gets_the_same_commit_sequence() {
    let app = app(Settings::default());
    let body = edit("r1", QUANTITY, &app.coordinate, Some(1.0));
    let first = call(&app, "SubmitEdit", body.clone()).await;
    assert_eq!(first, (StatusCode::OK, json!({ "sequence": 0 })));
    assert_eq!(call(&app, "SubmitEdit", body).await, first);
    assert_eq!(
        call(&app, "GetLag", json!({})).await.1["committed"],
        json!(0),
        "the resend took no Commit Sequence"
    );
}

#[tokio::test]
async fn an_edit_that_the_server_cannot_take_is_invalid_argument() {
    let app = app(Settings::default());
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
            "message": format!("measure {} is not an input", SALES.0),
        })
    );

    let mut no_value = edit("r2", QUANTITY, &app.coordinate, Some(1.0));
    no_value.as_object_mut().expect("an object").remove("value");
    let mut bad_ulid = edit("r3", QUANTITY, &app.coordinate, Some(1.0));
    bad_ulid["measure"] = json!("not a ULID");
    let mut unknown_field = edit("r4", QUANTITY, &app.coordinate, Some(1.0));
    unknown_field["note"] = json!("x");
    let long_request_id = "r".repeat(129);
    for body in [
        no_value,
        bad_ulid,
        unknown_field,
        edit("", QUANTITY, &app.coordinate, Some(1.0)),
        edit(&long_request_id, QUANTITY, &app.coordinate, Some(1.0)),
        edit("r5", QUANTITY, &app.coordinate[1..], Some(1.0)),
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
        edit("r6", QUANTITY, &app.coordinate, Some(1.0)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{failure}");
    assert_eq!(failure["code"], "invalid_argument");
    assert_eq!(
        call(&app, "GetLag", json!({})).await,
        (
            StatusCode::OK,
            json!({ "committed": null, "recalculated": null })
        ),
        "no edit entered the log"
    );
}

#[tokio::test(start_paused = true)]
async fn each_refusal_has_its_connect_code() {
    let app = app(Settings {
        busy_after: 0,
        calculation: Calculation::Inline(Duration::from_secs(10)),
    });
    let unknown = MeasureUlid(Ulid(999));
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
    assert_eq!(
        refusal(&app, "ReadCells", read_of(5, cell(SALES, &app.coordinate))).await,
        (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable"))
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
    let app = app(Settings::default());
    let (status, definitions) = call(&app, "ListDefinitions", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let measures = definitions["measures"].as_array().expect("measures");
    let kind = |ulid: MeasureUlid| {
        measures
            .iter()
            .find(|measure| measure["ulid"] == ulid.0.to_string())
            .map(|measure| measure["kind"].clone())
    };
    assert_eq!(kind(QUANTITY), Some(json!("input")));
    assert_eq!(kind(SALES), Some(json!("calculated")));
    let first_axis = &measures[0]["axes"][0];
    let dimension = definitions["dimensions"]
        .as_array()
        .expect("dimensions")
        .iter()
        .find(|dimension| &dimension["ulid"] == first_axis)
        .expect("the axis is a listed dimension");
    assert_eq!(dimension["members"][0]["id"], 0);
    assert!(dimension["members"][0]["key"].is_string());
}
