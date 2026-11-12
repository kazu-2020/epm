use std::sync::Arc;
use std::time::Duration;

use change_log::{Epoch, Start};
use datagen::representative::{self, Size};
use object_store::memory::InMemory;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use serde_json::{Value, json};
use server::http::Stopped;
use server::{Engine, Settings};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const IN_FLIGHT: Duration = Duration::from_millis(100);

struct Running {
    stopped: tokio::task::JoinHandle<Stopped>,
    shutdown: oneshot::Sender<()>,
    address: std::net::SocketAddr,
}

async fn run(put: Duration, deadline: Duration) -> Running {
    let config = ThrottleConfig {
        wait_put_per_call: put,
        ..ThrottleConfig::default()
    };
    let engine = Engine::new(representative::generate(&Size::TINY, 7)).expect("valid formulas");
    let (handle, owner) = server::start(
        engine,
        Start::default(),
        Settings::default(),
        Arc::new(ThrottledStore::new(InMemory::new(), config)),
        "ws".to_owned(),
        Epoch(1),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("an address");
    let (shutdown, signal) = oneshot::channel::<()>();
    let stopped = tokio::spawn(server::http::serve(
        listener,
        handle,
        owner,
        async move {
            let _ = signal.await;
        },
        deadline,
    ));
    Running {
        stopped,
        shutdown,
        address,
    }
}

async fn send_submit_edit(address: std::net::SocketAddr) -> TcpStream {
    let body = json!({
        "request_id": "r1",
        "measure": ulid::Ulid(100).to_string(),
        "coordinate": [0, 1, 1, 0, 1],
        "value": 3.0,
    })
    .to_string();
    let request = format!(
        "POST /epm.v1.Workspace/SubmitEdit HTTP/1.1\r\nhost: test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(address).await.expect("a connection");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request is sent");
    stream
}

#[tokio::test]
async fn a_shutdown_waits_for_the_edit_in_flight_and_then_stops_with_success() {
    let running = run(Duration::from_millis(500), Duration::from_secs(5)).await;
    let mut stream = send_submit_edit(running.address).await;
    tokio::time::sleep(IN_FLIGHT).await;
    running
        .shutdown
        .send(())
        .expect("the server waits for the signal");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("a response");
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    let (_, body) = response.split_once("\r\n\r\n").expect("a body");
    let body: Value = serde_json::from_str(body).expect("a JSON body");
    assert_eq!(body, json!({ "sequence": 0 }));
    let stopped = running.stopped.await.expect("serve does not panic");
    assert!(matches!(stopped, Stopped::Drained), "{stopped:?}");
}

#[tokio::test]
async fn a_shutdown_stops_with_the_deadline_when_a_request_does_not_finish() {
    let running = run(Duration::from_secs(60), Duration::from_millis(200)).await;
    let _stream = send_submit_edit(running.address).await;
    tokio::time::sleep(IN_FLIGHT).await;
    running
        .shutdown
        .send(())
        .expect("the server waits for the signal");

    let stopped = running.stopped.await.expect("serve does not panic");
    assert!(matches!(stopped, Stopped::Deadline), "{stopped:?}");
}
