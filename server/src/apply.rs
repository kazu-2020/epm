use std::sync::Arc;

use change_log::Commit;

use crate::{Engine, EngineAlarm};

pub(crate) enum Event {
    Committed(Vec<Commit>),
    Finished(Result<Engine, EngineAlarm>),
}

#[derive(Debug)]
pub(crate) enum Effect {
    Calculate {
        engine: Arc<Engine>,
        commits: Vec<Commit>,
    },
    Publish(Arc<Engine>),
    Alarm(EngineAlarm),
}

pub(crate) struct Applier {
    state: State,
}

enum State {
    Idle(Arc<Engine>),
    Calculating { pending: Vec<Commit> },
    Stopped,
}

impl Applier {
    pub(crate) fn new(engine: Arc<Engine>) -> Applier {
        Applier {
            state: State::Idle(engine),
        }
    }

    #[must_use]
    pub(crate) fn step(&mut self, event: Event) -> Vec<Effect> {
        let mut effects = Vec::new();
        let state = std::mem::replace(&mut self.state, State::Stopped);
        self.state = transition(state, event, &mut effects);
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

fn calculate(engine: Arc<Engine>, commits: Vec<Commit>, effects: &mut Vec<Effect>) -> State {
    effects.push(Effect::Calculate { engine, commits });
    State::Calculating {
        pending: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use change_log::{RequestId, Sequence};
    use datagen::representative::{self, Size};

    use super::*;

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(representative::generate(&Size::TINY, 7)).expect("valid formulas"))
    }

    fn commit(sequence: u64) -> Commit {
        Commit {
            request_id: RequestId(Bytes::from(format!("r{sequence}"))),
            sequence: Sequence(sequence),
            payload: Bytes::new(),
        }
    }

    fn committed(sequence: u64) -> Event {
        Event::Committed(vec![commit(sequence)])
    }

    fn sequences(commits: &[Commit]) -> Vec<u64> {
        commits.iter().map(|commit| commit.sequence.0).collect()
    }

    #[test]
    fn commits_that_arrive_during_a_calculation_form_the_next_batch() {
        let first = engine();
        let mut applier = Applier::new(Arc::clone(&first));
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
        let mut applier = Applier::new(engine());
        let effects = applier.step(Event::Committed((0..3).map(commit).collect()));
        let [Effect::Calculate { commits, .. }] = &effects[..] else {
            panic!("{effects:?}")
        };
        assert_eq!(sequences(commits), [0, 1, 2]);
    }

    #[test]
    fn an_empty_batch_starts_no_calculation() {
        let mut applier = Applier::new(engine());
        assert!(applier.step(Event::Committed(Vec::new())).is_empty());
        assert_eq!(
            applier.step(committed(0)).len(),
            1,
            "the applier is still idle"
        );
    }

    #[test]
    fn a_failed_calculation_raises_an_alarm_and_stops() {
        let mut applier = Applier::new(engine());
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
}
