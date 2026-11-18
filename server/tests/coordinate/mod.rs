use std::sync::LazyLock;

use datagen::representative::{self, Size};
use dimension::MemberIndex;
use measure::MeasureGlobalId;
use server::Engine;
use workspace::{Coordinate, Workspace};

static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
    Engine::new(
        Workspace::try_new(representative::generate(&Size::TINY, 7)).expect("a valid workspace"),
    )
    .expect("valid formulas")
});

pub(crate) fn named(measure: MeasureGlobalId, members: &[MemberIndex]) -> Coordinate {
    let measure = ENGINE
        .measures()
        .iter()
        .find(|candidate| candidate.global_id() == measure)
        .expect("the engine holds the measure");
    Coordinate::named(ENGINE.axes(measure), members).expect("the members exist")
}
