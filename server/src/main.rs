use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use change_log::Epoch;
use datagen::representative::{self, Size};
use object_store::aws::AmazonS3Builder;
use server::{Engine, Settings};

// shortcut: the workspace definition is fixed in code because the log does not hold it, put it in the log when users define their own.
const SIZE: Size = Size::TINY;
const SEED: u64 = 7;

struct Arguments {
    listen: String,
    workspace: String,
    epoch: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = match parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!(
                "{message}\nusage: server [--listen ADDRESS] [--workspace NAME] [--epoch NUMBER]"
            );
            return ExitCode::from(2);
        }
    };
    let Ok(bucket) = std::env::var("EPM_S3_BUCKET") else {
        eprintln!("set EPM_S3_BUCKET");
        return ExitCode::from(2);
    };
    let store = match AmazonS3Builder::from_env().with_bucket_name(bucket).build() {
        Ok(store) => Arc::new(store),
        Err(error) => {
            eprintln!("S3: {error}");
            return ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let engine = Engine::new(representative::generate(&SIZE, SEED)).expect("valid formulas");
    eprintln!("engine ready in {:?}", started.elapsed());
    let (handle, owner) = server::start(
        engine,
        Settings::default(),
        store,
        arguments.workspace.clone(),
        Epoch(arguments.epoch),
    );
    let listener = match tokio::net::TcpListener::bind(&arguments.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("listen on {}: {error}", arguments.listen);
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "workspace {} epoch {} listening on {}",
        arguments.workspace, arguments.epoch, arguments.listen
    );
    // Not a restart in this process after an exit: the same epoch must not write again (ADR 0006).
    tokio::select! {
        exit = owner => {
            eprintln!("the owner stopped: {exit:?}");
            ExitCode::FAILURE
        }
        served = axum::serve(listener, server::http::router(handle)) => {
            eprintln!("the HTTP server stopped: {served:?}");
            ExitCode::FAILURE
        }
        _ = tokio::signal::ctrl_c() => ExitCode::SUCCESS,
    }
}

fn parse(mut words: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut arguments = Arguments {
        listen: "127.0.0.1:8080".to_owned(),
        workspace: "demo".to_owned(),
        // Not the tail of the log: two processes that start together would always take the same epoch; the clock makes that need the same millisecond.
        epoch: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis()
            .try_into()
            .expect("the time in milliseconds fits in 64 bits"),
    };
    while let Some(flag) = words.next() {
        let value = words.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => arguments.listen = value,
            "--workspace" => arguments.workspace = value,
            "--epoch" => {
                arguments.epoch = value
                    .parse()
                    .map_err(|_| format!("--epoch {value:?} is not a number"))?
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(arguments)
}
