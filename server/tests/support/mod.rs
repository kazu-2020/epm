// Not a dead code warning: each test binary compiles this module and uses only some of it.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use change_log::{AssigneeId, Epoch, ProcessId, Start};
use datagen::representative::{self, Size};
use dimension::MemberIndex;
use measure::MeasureGlobalId;
use object_store::ObjectStore;
use server::{Engine, Exit, Handle, Settings};
use ulid::Ulid;
use workspace::{Coordinate, Workspace, WorkspaceGlobalId};

pub const WORKSPACE: WorkspaceGlobalId = WorkspaceGlobalId(Ulid(1));

pub fn engine() -> Engine {
    Engine::new(
        Workspace::try_new(representative::generate(&Size::TINY, 7)).expect("a valid workspace"),
    )
    .expect("valid formulas")
}

pub fn start(
    engine: Engine,
    start: Start,
    settings: Settings,
    store: Arc<dyn ObjectStore>,
    epoch: u64,
) -> (Handle, impl Future<Output = Exit> + use<>) {
    server::start(
        engine,
        start,
        settings,
        store,
        WORKSPACE,
        AssigneeId {
            epoch: Epoch(epoch),
            process: ProcessId(u128::from(epoch)),
        },
    )
}

pub async fn loaded(handle: &Handle) {
    tokio::time::timeout(Duration::from_secs(60), handle.wait_loaded())
        .await
        .expect("the assignee loads the change log in time")
        .expect("the assignee runs");
}

pub fn named(engine: &Engine, measure: MeasureGlobalId, members: &[MemberIndex]) -> Coordinate {
    let measure = engine
        .measures()
        .iter()
        .find(|candidate| candidate.global_id() == measure)
        .expect("the engine holds the measure");
    measure
        .definition()
        .axes()
        .iter()
        .zip(members)
        .map(|(&axis, &member)| {
            let dimension = engine
                .dimensions()
                .iter()
                .find(|dimension| dimension.global_id() == axis)
                .expect("the axis is a dimension");
            (
                axis,
                dimension
                    .member(member)
                    .expect("the member exists")
                    .global_id(),
            )
        })
        .collect()
}
