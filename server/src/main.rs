use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use change_log::{AssigneeId, Epoch, ProcessId};
use datagen::representative::{self, Size};
use object_store::aws::AmazonS3Builder;
use server::Settings;
use server::http::Shutdown;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use ulid::Ulid;
use workspace::{Workspace, WorkspaceGlobalId};

// shortcut: the workspace definition is fixed in code because the log does not hold it, put it in the log when users define their own.
const SIZE: Size = Size::TINY;
const SEED: u64 = 7;
// Not the 10 seconds of docker stop: SIGKILL comes then, and the process cannot report the missed deadline.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

struct Arguments {
    listen: String,
    workspace: WorkspaceGlobalId,
    epoch: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = match parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!(
                "{message}\nusage: server --workspace ULID [--listen ADDRESS] [--epoch NUMBER]"
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
            arguments.workspace,
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
        arguments.workspace.0, arguments.epoch, arguments.listen
    );
    let (handle, assignee) = server::start(
        engine,
        start,
        Settings::default(),
        store,
        arguments.workspace,
        AssigneeId {
            epoch: Epoch(arguments.epoch),
            process: ProcessId(Ulid::generate().0),
        },
    );
    match server::http::serve(listener, handle, assignee, shutdown, SHUTDOWN_DEADLINE).await {
        Shutdown::Drained => ExitCode::SUCCESS,
        Shutdown::Deadline => {
            eprintln!(
                "the requests did not finish in {SHUTDOWN_DEADLINE:?} after the shutdown signal"
            );
            ExitCode::FAILURE
        }
        // Not a restart in this process after an exit: the same epoch must not write again (ADR 0006).
        Shutdown::Assignee(exit) => {
            eprintln!("the assignee stopped: {exit:?}");
            ExitCode::FAILURE
        }
    }
}

fn parse(mut words: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut listen = "127.0.0.1:8080".to_owned();
    let mut workspace = None;
    // Not the tail of the log: two processes that start together would always take the same epoch; the clock makes that need the same millisecond.
    let mut epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis()
        .try_into()
        .expect("the time in milliseconds fits in 64 bits");
    while let Some(flag) = words.next() {
        let value = words.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--listen" => listen = value,
            "--workspace" => {
                let global_id = Ulid::from_string(&value)
                    .map_err(|_| format!("--workspace {value:?} is not a ULID"))?;
                workspace = Some(WorkspaceGlobalId(global_id));
            }
            "--epoch" => {
                epoch = value
                    .parse()
                    .map_err(|_| format!("--epoch {value:?} is not a number"))?
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(Arguments {
        listen,
        workspace: workspace.ok_or("--workspace is required")?,
        epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(words: &[&str]) -> Result<WorkspaceGlobalId, String> {
        parse(words.iter().map(|word| (*word).to_owned())).map(|arguments| arguments.workspace)
    }

    #[test]
    fn the_workspace_is_a_required_ulid() {
        assert_eq!(
            workspace(&["--listen", "127.0.0.1:0"]),
            Err("--workspace is required".to_owned())
        );
        assert_eq!(
            workspace(&["--workspace", "demo~"]),
            Err("--workspace \"demo~\" is not a ULID".to_owned())
        );
    }

    #[test]
    fn lowercase_and_uppercase_name_the_same_workspace() {
        let upper = workspace(&["--workspace", "01JCQ7ZK3M5V8W9X0Y1Z2A3B4C"]).expect("a ULID");
        let lower = workspace(&["--workspace", "01jcq7zk3m5v8w9x0y1z2a3b4c"]).expect("a ULID");
        assert_eq!(upper, lower);
        assert_eq!(upper.0.to_string(), "01JCQ7ZK3M5V8W9X0Y1Z2A3B4C");
    }
}
