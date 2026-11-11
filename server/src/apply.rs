use std::sync::Arc;

use change_log::{Commit, RequestId, Slot};

use crate::snapshot::Snapshot;
use crate::{Engine, EngineAlarm};

pub(crate) enum Event {
    Committed(Vec<(Slot, Commit)>),
    Finished(Result<Engine, EngineAlarm>),
    SnapshotWritten(Option<Slot>),
}

#[derive(Debug)]
pub(crate) enum Effect {
    Calculate {
        engine: Arc<Engine>,
        commits: Vec<(Slot, Commit)>,
    },
    Publish(Arc<Engine>),
    WriteSnapshot(Snapshot),
    Alarm(EngineAlarm),
}

pub(crate) struct Applier {
    state: State,
    request_ids: Vec<RequestId>,
    snapshot_every: u64,
    snapshot_slot: Slot,
    writing: bool,
}

enum State {
    Idle(Arc<Engine>),
    Calculating { pending: Vec<(Slot, Commit)> },
    Stopped,
}

impl Applier {
    pub(crate) fn new(
        engine: Arc<Engine>,
        request_ids: Vec<RequestId>,
        snapshot_every: u64,
    ) -> Applier {
        Applier {
            snapshot_slot: engine.slot,
            state: State::Idle(engine),
            request_ids,
            snapshot_every,
            writing: false,
        }
    }

    #[must_use]
    pub(crate) fn step(&mut self, event: Event) -> Vec<Effect> {
        match &event {
            Event::Committed(commits) => self
                .request_ids
                .extend(commits.iter().map(|(_, commit)| commit.request_id.clone())),
            Event::SnapshotWritten(written) => {
                self.writing = false;
                self.snapshot_slot = self.snapshot_slot.max(written.unwrap_or_default());
            }
            Event::Finished(_) => {}
        }
        let mut effects = Vec::new();
        let state = std::mem::replace(&mut self.state, State::Stopped);
        self.state = transition(state, event, &mut effects);
        let published = effects.iter().find_map(|effect| match effect {
            Effect::Publish(engine) => Some(Arc::clone(engine)),
            _ => None,
        });
        if let Some(engine) = published
            && !self.writing
            && engine.slot.0 >= self.snapshot_slot.0.saturating_add(self.snapshot_every)
        {
            self.writing = true;
            effects.push(Effect::WriteSnapshot(engine.snapshot(&self.request_ids)));
        }
        effects
    }
}

fn transition(state: State, event: Event, effects: &mut Vec<Effect>) -> State {
    match (state, event) {
        (State::Idle(engine), Event::Committed(commits)) if !commits.is_empty() => {
            calculate(engine, commits, effects)
        }
        (State::Calculating { mut pending }, Event::Committed(commits)) => {
            pending.extend(commits);
            State::Calculating { pending }
        }
        (State::Calculating { pending }, Event::Finished(Ok(engine))) => {
            let engine = Arc::new(engine);
            effects.push(Effect::Publish(Arc::clone(&engine)));
            if pending.is_empty() {
                return State::Idle(engine);
            }
            // Not cut into smaller batches: every commit is a cell edit today. A definition change (#88) will end a batch here.
            calculate(engine, pending, effects)
        }
        (State::Calculating { .. }, Event::Finished(Err(alarm))) => {
            effects.push(Effect::Alarm(alarm));
            State::Stopped
        }
        (state, _) => state,
    }
}

fn calculate(
    engine: Arc<Engine>,
    commits: Vec<(Slot, Commit)>,
    effects: &mut Vec<Effect>,
) -> State {
    effects.push(Effect::Calculate { engine, commits });
    State::Calculating {
        pending: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use change_log::{RequestId, Sequence, Start};

    use crate::snapshot::Snapshot;
    use datagen::representative::{self, Size};

    use super::*;

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(representative::generate(&Size::TINY, 7)).expect("valid formulas"))
    }

    fn commit(sequence: u64) -> (Slot, Commit) {
        (
            Slot(sequence),
            Commit {
                request_id: RequestId(Bytes::from(format!("r{sequence}"))),
                sequence: Sequence(sequence),
                payload: Bytes::new(),
            },
        )
    }

    fn applier(engine: Arc<Engine>) -> Applier {
        Applier::new(engine, Vec::new(), u64::MAX)
    }

    fn committed(sequence: u64) -> Event {
        Event::Committed(vec![commit(sequence)])
    }

    fn sequences(commits: &[(Slot, Commit)]) -> Vec<u64> {
        commits
            .iter()
            .map(|(_, commit)| commit.sequence.0)
            .collect()
    }

    #[test]
    fn commits_that_arrive_during_a_calculation_form_the_next_batch() {
        let first = engine();
        let mut applier = applier(Arc::clone(&first));
        let effects = applier.step(committed(0));
        let [Effect::Calculate { engine, commits }] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert!(Arc::ptr_eq(engine, &first));
        assert_eq!(sequences(commits), [0]);
        assert!(applier.step(committed(1)).is_empty());
        assert!(applier.step(committed(2)).is_empty());

        let effects = applier.step(Event::Finished(Ok((*first).clone())));
        let [
            Effect::Publish(published),
            Effect::Calculate { engine, commits },
        ] = &effects[..]
        else {
            panic!("{effects:?}")
        };
        assert!(Arc::ptr_eq(engine, published), "from the published engine");
        assert_eq!(sequences(commits), [1, 2]);
        let second = Arc::clone(published);

        let effects = applier.step(Event::Finished(Ok((*first).clone())));
        let [Effect::Publish(published)] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert!(!Arc::ptr_eq(published, &second));
        let third = Arc::clone(published);
        let effects = applier.step(committed(3));
        let [Effect::Calculate { engine, commits }] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert!(
            Arc::ptr_eq(engine, &third),
            "an idle applier starts at once"
        );
        assert_eq!(sequences(commits), [3]);
    }

    #[test]
    fn an_idle_applier_calculates_the_commits_that_arrived_together_in_one_batch() {
        let mut applier = applier(engine());
        let effects = applier.step(Event::Committed((0..3).map(commit).collect()));
        let [Effect::Calculate { commits, .. }] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(sequences(commits), [0, 1, 2]);
    }

    #[test]
    fn an_empty_batch_starts_no_calculation() {
        let mut applier = applier(engine());
        assert!(applier.step(Event::Committed(Vec::new())).is_empty());
        assert_eq!(
            applier.step(committed(0)).len(),
            1,
            "the applier is still idle"
        );
    }

    #[test]
    fn a_failed_calculation_raises_an_alarm_and_stops() {
        let mut applier = applier(engine());
        assert_eq!(applier.step(committed(0)).len(), 1);
        assert!(applier.step(committed(1)).is_empty());
        let alarm = EngineAlarm::OutOfOrder {
            expected: Sequence(0),
            found: Sequence(1),
        };
        let effects = applier.step(Event::Finished(Err(alarm)));
        let [Effect::Alarm(raised)] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(
            *raised,
            EngineAlarm::OutOfOrder {
                expected: Sequence(0),
                found: Sequence(1),
            }
        );
        assert!(
            applier.step(committed(2)).is_empty(),
            "a stopped applier does nothing"
        );
    }

    fn edit(sequence: u64, slot: u64, engine: &Engine) -> (Slot, Commit) {
        let input = &engine.measures()[0];
        let edit = crate::Edit {
            measure: input.ulid(),
            coordinate: vec![dimension::MemberIndex(0); input.definition().axes.len()],
            value: Some(sequence as f64),
        };
        let (_, commit) = commit(sequence);
        (
            Slot(slot),
            Commit {
                payload: edit.encode(),
                ..commit
            },
        )
    }

    fn applied(applier: &mut Applier, effects: Vec<Effect>) -> Vec<Effect> {
        let [Effect::Calculate { engine, commits }] = &effects[..] else {
            panic!("{effects:?}")
        };
        applier.step(Event::Finished(engine.apply_batch(commits)))
    }

    fn written(effects: &[Effect]) -> Option<(Slot, usize)> {
        effects.iter().find_map(|effect| match effect {
            Effect::WriteSnapshot(Snapshot { start, .. }) => {
                Some((start.slot, start.request_ids.len()))
            }
            _ => None,
        })
    }

    fn published(applier: &mut Applier, engine: &Engine, sequence: u64, slot: u64) -> Vec<Effect> {
        let effects = applier.step(Event::Committed(vec![edit(sequence, slot, engine)]));
        applied(applier, effects)
    }

    #[test]
    fn a_snapshot_is_written_after_a_publish_once_the_slot_moved_far_enough() {
        let first = engine();
        let mut applier = Applier::new(Arc::clone(&first), Vec::new(), 2);
        assert_eq!(written(&published(&mut applier, &first, 0, 0)), None);
        assert_eq!(
            written(&published(&mut applier, &first, 1, 2)),
            Some((Slot(2), 2)),
            "two slots after the start"
        );
        assert_eq!(
            written(&published(&mut applier, &first, 2, 4)),
            None,
            "the write of slot 2 still runs"
        );
        assert!(
            applier
                .step(Event::SnapshotWritten(Some(Slot(2))))
                .is_empty()
        );
        assert_eq!(
            written(&published(&mut applier, &first, 3, 5)),
            Some((Slot(5), 4))
        );
        assert!(applier.step(Event::SnapshotWritten(None)).is_empty());
        let effects = applier.step(Event::Committed(vec![edit(4, 6, &first)]));
        assert!(
            applier
                .step(Event::Committed(vec![edit(5, 7, &first)]))
                .is_empty()
        );
        assert_eq!(
            written(&applied(&mut applier, effects)),
            Some((Slot(6), 5)),
            "a failed write is tried again, and holds only the published commits"
        );
    }

    #[test]
    fn no_snapshot_is_written_when_the_interval_is_the_largest_count() {
        let start = Start {
            slot: Slot(5),
            ..Start::default()
        };
        let restored = Arc::new(
            Engine::resume(representative::generate(&Size::TINY, 7), &start)
                .expect("valid formulas"),
        );
        let mut applier = applier(Arc::clone(&restored));
        assert_eq!(written(&published(&mut applier, &restored, 0, 6)), None);
    }
}
