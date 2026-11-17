use std::sync::LazyLock;

use datagen::representative::{self, Size};
use dimension::MemberIndex;
use measure::MeasureGlobalId;
use server::Engine;
use workspace::{Coordinate, Workspace};

// Not a fresh engine for each call: the tests name a coordinate many times, and building an engine calculates every formula.
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
