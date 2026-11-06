use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use change_log::{Alarm, Epoch, Exit, serve};
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, RetryConfig};
use tokio::sync::mpsc;

const EMPTY_LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult><IsTruncated>false</IsTruncated><KeyCount>0</KeyCount></ListBucketResult>"#;

#[derive(Clone, Copy)]
struct Statuses {
    list: u16,
    put: u16,
    get: u16,
}

fn fake_s3(statuses: Statuses) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let endpoint = format!("http://{}", listener.local_addr().expect("an address"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("a connection");
            let mut reader = BufReader::new(stream.try_clone().expect("a clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("a request line");
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("a header");
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
                EMPTY_LIST
            } else {
                ""
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

fn store(endpoint: &str) -> Arc<dyn ObjectStore> {
    Arc::new(
        AmazonS3Builder::new()
            .with_bucket_name("bucket")
            .with_region("us-east-1")
            .with_endpoint(endpoint)
            .with_allow_http(true)
            .with_access_key_id("key")
            .with_secret_access_key("secret")
            .with_retry(RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            })
            .build()
            .expect("an S3 client"),
    )
}

async fn run(statuses: Statuses, wait: Duration) -> (Option<Exit>, Vec<String>) {
    let (endpoint, seen) = fake_s3(statuses);
    let (_requests, receiver) = mpsc::channel(1);
    let exit = tokio::time::timeout(
        wait,
        serve(store(&endpoint), "ws", Epoch(1), receiver, |_| false),
    )
    .await
    .ok();
    let seen = seen.lock().expect("the record").clone();
    (exit, seen)
}

#[tokio::test]
async fn a_permanent_error_stops_the_owner_with_an_alarm() {
    let cases = [
        (
            Statuses {
                list: 403,
                put: 200,
                get: 200,
            },
            "ws/log",
            403,
            &["LIST"][..],
        ),
        (
            Statuses {
                list: 401,
                put: 200,
                get: 200,
            },
            "ws/log",
            401,
            &["LIST"],
        ),
        (
            Statuses {
                list: 501,
                put: 200,
                get: 200,
            },
            "ws/log",
            501,
            &["LIST"],
        ),
        (
            Statuses {
                list: 200,
                put: 403,
                get: 200,
            },
            "ws/log/00000000000000000000",
            403,
            &["LIST", "PUT"],
        ),
        (
            Statuses {
                list: 200,
                put: 500,
                get: 403,
            },
            "ws/log/00000000000000000000",
            403,
            &["LIST", "PUT", "GET"],
        ),
    ];
    for (statuses, key, status, operations) in cases {
        let (exit, seen) = run(statuses, Duration::from_secs(5)).await;
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
    let (exit, seen) = run(
        Statuses {
            list: 503,
            put: 200,
            get: 200,
        },
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(exit, None, "the owner still runs");
    assert!(seen.len() > 1, "LIST is sent again: {seen:?}");
}
