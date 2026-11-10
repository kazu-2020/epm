use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use change_log::{AdmitAll, Alarm, Epoch, Exit, serve};
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::{ObjectStore, RetryConfig};
use tokio::sync::mpsc;

const EMPTY_LIST: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>"#,
    "<ListBucketResult><IsTruncated>false</IsTruncated><KeyCount>0</KeyCount></ListBucketResult>",
);

#[derive(Clone, Copy)]
struct Statuses {
    list: u16,
    put: u16,
    get: u16,
    code: &'static str,
}

fn fake_s3(statuses: Statuses) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let endpoint = format!("http://{}", listener.local_addr().expect("an address"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        'connections: for stream in listener.incoming() {
            let mut stream = stream.expect("a connection");
            let mut reader = BufReader::new(stream.try_clone().expect("a clone"));
            let mut line = String::new();
            if reader.read_line(&mut line).expect("a request line") == 0 {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).expect("a header") == 0 {
                    continue 'connections;
                }
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().expect("a length");
                }
            }
            reader.read_exact(&mut vec![0; length]).expect("the body");
            let (operation, status) = if line.contains("list-type=2") {
                ("LIST", statuses.list)
            } else if line.starts_with("PUT") {
                ("PUT", statuses.put)
            } else {
                ("GET", statuses.get)
            };
            record
                .lock()
                .expect("the record")
                .push(operation.to_owned());
            let body = if operation == "LIST" && status == 200 {
                EMPTY_LIST.to_owned()
            } else if statuses.code.is_empty() {
                String::new()
            } else {
                format!("<Error><Code>{}</Code></Error>", statuses.code)
            };
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("a response");
        }
    });
    (endpoint, seen)
}

fn store(endpoint: &str, conditional_put: bool) -> Arc<dyn ObjectStore> {
    let conditional_put = if conditional_put {
        S3ConditionalPut::ETagMatch
    } else {
        S3ConditionalPut::Disabled
    };
    Arc::new(
        AmazonS3Builder::new()
            .with_bucket_name("bucket")
            .with_region("us-east-1")
            .with_endpoint(endpoint)
            .with_allow_http(true)
            .with_access_key_id("key")
            .with_secret_access_key("secret")
            .with_conditional_put(conditional_put)
            .with_retry(RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            })
            .build()
            .expect("an S3 client"),
    )
}

async fn run(
    statuses: Statuses,
    conditional_put: bool,
    wait: Duration,
) -> (Option<Exit>, Vec<String>) {
    let (endpoint, seen) = fake_s3(statuses);
    let (_requests, receiver) = mpsc::channel(1);
    let (committed, _passed_on) = mpsc::unbounded_channel();
    let exit = tokio::time::timeout(
        wait,
        serve(
            store(&endpoint, conditional_put),
            "ws",
            Epoch(1),
            AdmitAll,
            receiver,
            committed,
        ),
    )
    .await
    .ok();
    let seen = seen.lock().expect("the record").clone();
    (exit, seen)
}

fn statuses(list: u16, put: u16, get: u16) -> Statuses {
    Statuses {
        list,
        put,
        get,
        code: "",
    }
}

fn with_code(statuses: Statuses, code: &'static str) -> Statuses {
    Statuses { code, ..statuses }
}

#[tokio::test]
async fn a_permanent_error_stops_the_owner_with_an_alarm() {
    let slot = "ws/log/00000000000000000000";
    let cases = [
        (statuses(403, 200, 200), true, "ws/log", 403, &["LIST"][..]),
        (statuses(401, 200, 200), true, "ws/log", 401, &["LIST"]),
        (statuses(404, 200, 200), true, "ws/log", 404, &["LIST"]),
        (statuses(501, 200, 200), true, "ws/log", 501, &["LIST"]),
        (statuses(301, 200, 200), true, "ws/log", 301, &["LIST"]),
        (
            with_code(statuses(400, 200, 200), "AuthorizationHeaderMalformed"),
            true,
            "ws/log",
            400,
            &["LIST"],
        ),
        (statuses(200, 403, 200), true, slot, 403, &["LIST", "PUT"]),
        (
            with_code(statuses(200, 400, 200), "AuthorizationHeaderMalformed"),
            true,
            slot,
            400,
            &["LIST", "PUT"],
        ),
        (
            statuses(200, 500, 301),
            true,
            slot,
            301,
            &["LIST", "PUT", "GET"],
        ),
        (
            statuses(200, 500, 403),
            true,
            slot,
            403,
            &["LIST", "PUT", "GET"],
        ),
        (statuses(200, 200, 200), false, slot, 501, &["LIST"]),
    ];
    for (statuses, conditional_put, key, status, operations) in cases {
        let (exit, seen) = run(statuses, conditional_put, Duration::from_secs(5)).await;
        assert_eq!(
            exit,
            Some(Exit::Alarm(Alarm::PermanentStoreError {
                key: key.to_owned(),
                status
            })),
            "{seen:?}"
        );
        assert_eq!(seen, operations);
    }
}

#[tokio::test]
async fn a_transient_error_is_retried() {
    let cases = [
        (statuses(503, 200, 200), "LIST", "a server error"),
        (statuses(200, 500, 404), "PUT", "a GET of a missing key"),
        (
            with_code(statuses(400, 200, 200), "ExpiredToken"),
            "LIST",
            "an expired token",
        ),
        (
            with_code(statuses(400, 200, 200), "RequestTimeout"),
            "LIST",
            "a request timeout",
        ),
    ];
    for (statuses, operation, reason) in cases {
        let (exit, seen) = run(statuses, true, Duration::from_millis(500)).await;
        assert_eq!(exit, None, "{reason}: the owner still runs");
        let sent = seen.iter().filter(|seen| *seen == operation).count();
        assert!(sent > 1, "{reason}: {operation} is sent again: {seen:?}");
    }
}
