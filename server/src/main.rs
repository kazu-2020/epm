use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use change_log::{AssigneeId, Epoch, ProcessId};
use datagen::representative::{self, Size};
use object_store::aws::AmazonS3Builder;
use server::Settings;
use server::http::Shutdown;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use ulid::Ulid;
use workspace::{Workspace, WorkspaceGlobalId};

// shortcut: the workspace definition is fixed in code because the log does not hold it, put it in the log when users define their own.
const SIZE: Size = Size::TINY;
const SEED: u64 = 7;
// Not the 10 seconds of docker stop: SIGKILL comes then, and the process cannot report the missed deadline.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);
const USAGE: &str = "usage: server --workspace ULID --epoch NUMBER [--listen ADDRESS]";

struct Arguments {
    listen: String,
    workspace: WorkspaceGlobalId,
    epoch: Epoch,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_writer(std::io::stderr)
        .init();
    let arguments = match parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let Ok(bucket) = std::env::var("EPM_S3_BUCKET") else {
        error!("set EPM_S3_BUCKET");
        return ExitCode::from(2);
    };
    let store = match AmazonS3Builder::from_env().with_bucket_name(bucket).build() {
        Ok(store) => Arc::new(store),
        Err(error) => {
            error!(%error, "S3");
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
        warn!("stopped by a second signal before the requests finished");
        std::process::exit(1);
    });
    let mut shutdown = Box::pin(async move {
        let _ = signalled.await;
    });
    let starting = async {
        let started = Instant::now();
        let (engine, start) = server::restore(
            store.as_ref(),
            arguments.workspace,
            Workspace::try_new(representative::generate(&SIZE, SEED)).expect("a valid workspace"),
        )
        .await
        .expect("valid formulas");
        info!(
            elapsed = ?started.elapsed(),
            commit_sequence = start.request_ids.len(),
            "engine ready"
        );
        tokio::net::TcpListener::bind(&arguments.listen)
            .await
            .map(|listener| (engine, start, listener))
    };
    let (engine, start, listener) = tokio::select! {
        started = starting => match started {
            Ok(started) => started,
            Err(error) => {
                error!(listen = %arguments.listen, %error, "listen");
                return ExitCode::FAILURE;
            }
        },
        () = &mut shutdown => {
            info!("stopped by a signal during the start");
            return ExitCode::SUCCESS;
        }
    };
    info!(
        workspace = %arguments.workspace,
        epoch = arguments.epoch.0,
        listen = %arguments.listen,
        "listening"
    );
    let (handle, assignee) = server::start(
        engine,
        start,
        Settings::default(),
        store,
        arguments.workspace,
        AssigneeId {
            epoch: arguments.epoch,
            process: ProcessId(Ulid::generate().0),
        },
    );
    match server::http::serve(listener, handle, assignee, shutdown, SHUTDOWN_DEADLINE).await {
        Shutdown::Drained => ExitCode::SUCCESS,
        Shutdown::Deadline => {
            error!(
                deadline = ?SHUTDOWN_DEADLINE,
                "the requests did not finish before the deadline after the shutdown signal"
            );
            ExitCode::FAILURE
        }
        // Not a restart in this process after an exit: the same epoch must not write again (ADR 0006).
        Shutdown::Assignee(exit) => {
            error!(?exit, "the assignee stopped");
            ExitCode::FAILURE
        }
    }
}

fn parse(mut words: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut listen = "127.0.0.1:8080".to_owned();
    let mut workspace = None;
    // Not a default from the clock: the conditional UPDATE of the placement table issues each epoch once (ADR 0012), and a clock can repeat or go back.
    let mut epoch = None;
    while let Some(flag) = words.next() {
        let value = words.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => listen = value,
            "--workspace" => {
                workspace = Some(
                    value
                        .parse()
                        .map_err(|error| format!("--workspace {value:?}: {error}"))?,
                );
            }
            "--epoch" => {
                epoch = Some(Epoch(
                    value
                        .parse()
                        .map_err(|error| format!("--epoch {value:?}: {error}"))?,
                ));
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(Arguments {
        listen,
        workspace: workspace.ok_or("--workspace is required")?,
        epoch: epoch.ok_or("--epoch is required")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(words: &[&str]) -> Result<WorkspaceGlobalId, String> {
        parse(words.iter().map(|word| (*word).to_owned())).map(|arguments| arguments.workspace)
    }

    fn epoch(words: &[&str]) -> Result<Epoch, String> {
        parse(words.iter().map(|word| (*word).to_owned())).map(|arguments| arguments.epoch)
    }

    const WORKSPACE: &str = "01JCQ7ZK3M5V8W9X0Y1Z2A3B4C";

    #[test]
    fn the_workspace_is_a_required_ulid() {
        assert_eq!(
            workspace(&["--listen", "127.0.0.1:0", "--epoch", "1"]),
            Err("--workspace is required".to_owned())
        );
        assert_eq!(
            workspace(&["--workspace", "demo~", "--epoch", "1"]),
            Err("--workspace \"demo~\": not a ULID".to_owned())
        );
    }

    #[test]
    fn the_epoch_is_a_required_number() {
        assert_eq!(
            epoch(&["--workspace", WORKSPACE]),
            Err("--epoch is required".to_owned())
        );
        assert_eq!(
            epoch(&["--workspace", WORKSPACE, "--epoch", "one"]),
            Err("--epoch \"one\": invalid digit found in string".to_owned())
        );
        assert_eq!(
            epoch(&["--workspace", WORKSPACE, "--epoch", "-1"]),
            Err("--epoch \"-1\": invalid digit found in string".to_owned())
        );
        assert_eq!(
            epoch(&["--workspace", WORKSPACE, "--epoch", "18446744073709551616"]),
            Err(
                "--epoch \"18446744073709551616\": number too large to fit in target type"
                    .to_owned()
            )
        );
        assert_eq!(
            epoch(&["--workspace", WORKSPACE, "--epoch", "3"]),
            Ok(Epoch(3))
        );
    }
}
