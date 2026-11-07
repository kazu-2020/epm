mod common;

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, RetryConfig};
use tokio::runtime::Runtime;

struct ChangeLog;

impl common::Server for ChangeLog {}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn s3(bucket: String) -> AmazonS3Builder {
    AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(true)
        .with_retry(RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        })
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

#[test]
#[ignore = "needs CHANGE_LOG_S3_BUCKET, and AWS_ENDPOINT for a store other than AWS"]
fn seeds_keep_the_invariants_on_s3() {
    let bucket = env("CHANGE_LOG_S3_BUCKET").expect("CHANGE_LOG_S3_BUCKET");
    let prefix = env("CHANGE_LOG_S3_PREFIX").unwrap_or_else(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970");
        format!("change-log-test/local-{}", now.as_millis())
    });
    let seeds: u64 = env("CHANGE_LOG_S3_SEEDS").map_or(3, |seeds| seeds.parse().expect("a number"));
    let store: Arc<dyn ObjectStore> = Arc::new(s3(bucket).build().expect("an S3 client"));
    let runtime = runtime();
    for seed in 0..seeds {
        let workspace = format!("{prefix}/seed-{seed}");
        let started = Instant::now();
        let simulation =
            common::simulate(Arc::new(ChangeLog), Arc::clone(&store), &workspace, seed);
        let outcome = runtime.block_on(simulation);
        let (acknowledged, slots) = common::counts(&outcome);
        let violations = common::check(&outcome);
        println!(
            "{workspace}: {:.1?}, {acknowledged} requests acknowledged, {slots} slots written, witnessed {:?}",
            started.elapsed(),
            common::witnesses(&outcome),
        );
        assert!(violations.is_empty(), "{workspace}: {violations:?}");
    }
}
