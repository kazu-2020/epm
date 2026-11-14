use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use change_log::{Epoch, Start};
use datagen::representative::{self, Size};
use futures_util::stream::BoxStream;
use measure::MeasureGlobalId;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use serde_json::{Value, json};
use server::http::Stopped;
use server::{Engine, Settings};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};
use ulid::Ulid;
use workspace::Workspace;

const QUANTITY: MeasureGlobalId = MeasureGlobalId(Ulid(100));

#[derive(Debug, Default)]
struct Gate {
    inner: InMemory,
    puts: AtomicUsize,
    started: Notify,
    released: Notify,
}

impl fmt::Display for Gate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Gate")
    }
}

#[async_trait]
impl ObjectStore for Gate {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        // Not every PUT: the first one is the fence of the new owner, and the edit comes after it.
        if self.puts.fetch_add(1, Ordering::SeqCst) == 0 {
            return self.inner.put_opts(location, payload, options).await;
        }
        self.started.notify_one();
        self.released.notified().await;
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

struct Running {
    stopped: tokio::task::JoinHandle<Stopped>,
    shutdown: oneshot::Sender<()>,
    address: SocketAddr,
    store: Arc<Gate>,
}

async fn run(deadline: Duration) -> Running {
    let engine = Engine::new(
        Workspace::try_new(representative::generate(&Size::TINY, 7)).expect("a valid workspace"),
    )
    .expect("valid formulas");
    let store = Arc::new(Gate::default());
    let (handle, owner) = server::start(
        engine,
        Start::default(),
        Settings::default(),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
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
        store,
    }
}

async fn send_submit_edit(address: SocketAddr) -> TcpStream {
    let body = json!({
        "request_id": "r1",
        "measure": QUANTITY.0.to_string(),
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
    let running = run(Duration::from_secs(60)).await;
    let mut stream = send_submit_edit(running.address).await;
    running.store.started.notified().await;
    running
        .shutdown
        .send(())
        .expect("the server waits for the signal");
    // Not a release at once: the PUT could end before the server sees the signal, and a server that does not wait would pass.
    tokio::time::timeout(Duration::from_secs(5), async {
        while TcpStream::connect(running.address).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server stops taking connections");
    running.store.released.notify_one();

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
    let running = run(Duration::from_millis(200)).await;
    let _stream = send_submit_edit(running.address).await;
    running.store.started.notified().await;
    running
        .shutdown
        .send(())
        .expect("the server waits for the signal");

    let stopped = running.stopped.await.expect("serve does not panic");
    assert!(matches!(stopped, Stopped::Deadline), "{stopped:?}");
}
