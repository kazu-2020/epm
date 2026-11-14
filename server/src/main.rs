use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use change_log::Epoch;
use datagen::representative::{self, Size};
use object_store::aws::AmazonS3Builder;
use server::Settings;
use server::http::Shutdown;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use workspace::Workspace;

// shortcut: the workspace definition is fixed in code because the log does not hold it, put it in the log when users define their own.
const SIZE: Size = Size::TINY;
const SEED: u64 = 7;
// Not the 10 seconds of docker stop: SIGKILL comes then, and the process cannot report the missed deadline.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

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
    // Not after the restore: as PID 1 in a container, the process ignores SIGTERM until it has a handler.
    let mut interrupt = signal(SignalKind::interrupt()).expect("a handler for SIGINT");
    let mut terminate = signal(SignalKind::terminate()).expect("a handler for SIGTERM");
    let (first_signal, signalled) = oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        let _ = first_signal.send(());
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        eprintln!("stopped by a second signal before the requests finished");
        std::process::exit(1);
    });
    let mut shutdown = Box::pin(async move {
        let _ = signalled.await;
    });
    let starting = async {
        let started = Instant::now();
        let (engine, start) = server::restore(
            store.as_ref(),
            &arguments.workspace,
            Workspace::try_new(representative::generate(&SIZE, SEED)).expect("a valid workspace"),
        )
        .await
        .expect("valid formulas");
        eprintln!(
            "engine ready in {:?} from Commit Sequence {}",
            started.elapsed(),
            start.request_ids.len()
        );
        tokio::net::TcpListener::bind(&arguments.listen)
            .await
            .map(|listener| (engine, start, listener))
    };
    let (engine, start, listener) = tokio::select! {
        started = starting => match started {
            Ok(started) => started,
            Err(error) => {
                eprintln!("listen on {}: {error}", arguments.listen);
                return ExitCode::FAILURE;
            }
        },
        () = &mut shutdown => {
            eprintln!("stopped by a signal during the start");
            return ExitCode::SUCCESS;
        }
    };
    eprintln!(
        "workspace {} epoch {} listening on {}",
        arguments.workspace, arguments.epoch, arguments.listen
    );
    let (handle, owner) = server::start(
        engine,
        start,
        Settings::default(),
        store,
        arguments.workspace,
        Epoch(arguments.epoch),
    );
    match server::http::serve(listener, handle, owner, shutdown, SHUTDOWN_DEADLINE).await {
        Shutdown::Drained => ExitCode::SUCCESS,
        Shutdown::Deadline => {
            eprintln!(
                "the requests did not finish in {SHUTDOWN_DEADLINE:?} after the shutdown signal"
            );
            ExitCode::FAILURE
        }
        // Not a restart in this process after an exit: the same epoch must not write again (ADR 0006).
        Shutdown::Owner(exit) => {
            eprintln!("the owner stopped: {exit:?}");
            ExitCode::FAILURE
        }
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
