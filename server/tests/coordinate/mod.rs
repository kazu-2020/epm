use dimension::MemberIndex;
use measure::MeasureGlobalId;
use server::Engine;
use workspace::Coordinate;

pub(crate) fn named(
    engine: &Engine,
    measure: MeasureGlobalId,
    members: &[MemberIndex],
) -> Coordinate {
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
