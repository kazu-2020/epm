use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;

use bytes::Bytes;

use crate::segment::{self, Commit, DecodeError, Epoch, RequestId, Segment, Sequence, Slot};

const MAX_IN_FLIGHT: usize = if cfg!(feature = "two_in_flight") {
    2
} else {
    1
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub request_id: RequestId,
    pub payload: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutFailure {
    AlreadyExists,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Loaded { slots: BTreeMap<Slot, Segment> },
    Request(Request),
    Flush,
    PutSucceeded { slot: Slot },
    PutFailed { slot: Slot, failure: PutFailure },
    ReadBack { slot: Slot, found: Option<Segment> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect<R> {
    Put {
        slot: Slot,
        body: Bytes,
        attempt: u32,
    },
    Get {
        slot: Slot,
    },
    Committed(Commit),
    Ack {
        request_id: RequestId,
        sequence: Sequence,
    },
    Rejected {
        request_id: RequestId,
        reason: R,
    },
    Stop,
    Alarm(Alarm),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alarm {
    OlderEpochAfterDataWrite { slot: Slot, found: Epoch },
    OtherContentInOwnEpoch { slot: Slot },
    UnreadableSegment { slot: Slot, error: DecodeError },
    UnexpectedKey { key: String },
    PermanentStoreError { key: String, status: u16 },
}

pub trait Admit {
    type Rejection: Clone;

    fn admit(&mut self, payload: &Bytes) -> Result<(), Self::Rejection>;

    fn replay(&mut self, payload: &Bytes);
}

#[derive(Debug)]
pub struct AdmitAll;

impl Admit for AdmitAll {
    type Rejection = Infallible;

    fn admit(&mut self, _payload: &Bytes) -> Result<(), Infallible> {
        Ok(())
    }

    fn replay(&mut self, _payload: &Bytes) {}
}

#[derive(Debug)]
pub struct Owner<A> {
    epoch: Epoch,
    admit: A,
    phase: Phase,
}

#[derive(Debug)]
enum Phase {
    Loading {
        waiting: Vec<Request>,
    },
    Fencing {
        view: View,
        fence: Write,
        waiting: Vec<Request>,
    },
    Ready(Ready),
    Stopped,
}

#[derive(Debug)]
struct Ready {
    view: View,
    next: Slot,
    batch: Vec<Request>,
    in_flight: [Option<Write>; MAX_IN_FLIGHT],
}

#[derive(Debug, Default)]
struct View {
    sequences: HashMap<RequestId, Sequence>,
    next_sequence: Sequence,
}

#[derive(Debug)]
struct Write {
    slot: Slot,
    commits: Vec<Commit>,
    body: Bytes,
    attempt: u32,
    awaiting: Awaiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Awaiting {
    Reply,
    ReadBack,
}

enum Outcome {
    Waiting(Write),
    Committed(Write),
    OlderEpoch(Write, Segment),
    NewerEpoch,
    OtherContent(Write),
}

impl<A: Admit> Owner<A> {
    pub fn new(epoch: Epoch, admit: A) -> Owner<A> {
        Owner {
            epoch,
            admit,
            phase: Phase::Loading {
                waiting: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn step(&mut self, event: Event) -> Vec<Effect<A::Rejection>> {
        let mut effects = Vec::new();
        let phase = std::mem::replace(&mut self.phase, Phase::Stopped);
        self.phase = transition(self.epoch, &mut self.admit, phase, event, &mut effects);
        effects
    }
}

fn transition<A: Admit>(
    epoch: Epoch,
    admit: &mut A,
    phase: Phase,
    event: Event,
    effects: &mut Vec<Effect<A::Rejection>>,
) -> Phase {
    match (phase, event) {
        (Phase::Loading { waiting }, Event::Loaded { slots }) => {
            load(epoch, admit, &slots, waiting, effects)
        }
        (Phase::Loading { mut waiting }, Event::Request(request)) => {
            waiting.push(request);
            Phase::Loading { waiting }
        }
        (
            Phase::Fencing {
                view,
                fence,
                mut waiting,
            },
            Event::Request(request),
        ) => {
            waiting.push(request);
            Phase::Fencing {
                view,
                fence,
                waiting,
            }
        }
        (
            Phase::Fencing {
                mut view,
                fence,
                waiting,
            },
            event,
        ) if fence.answered_by(&event) => match advance(epoch, fence, event, effects) {
            Outcome::Waiting(fence) => Phase::Fencing {
                view,
                fence,
                waiting,
            },
            Outcome::Committed(fence) => ready(view, next(fence.slot), waiting, admit, effects),
            Outcome::OlderEpoch(taken, found) => {
                view.add_older(&found.commits, admit, effects);
                let fence = Write::new(epoch, next(taken.slot), Vec::new());
                effects.push(fence.put());
                Phase::Fencing {
                    view,
                    fence,
                    waiting,
                }
            }
            Outcome::NewerEpoch => stop(effects),
            Outcome::OtherContent(fence) => other_content(fence, effects),
        },
        (Phase::Ready(mut ready), Event::Request(request)) => {
            ready.receive(request, admit, effects);
            Phase::Ready(ready)
        }
        (Phase::Ready(mut ready), Event::Flush) => {
            ready.flush(epoch, effects);
            Phase::Ready(ready)
        }
        (Phase::Ready(mut ready), event) => {
            let Some(cell) = ready
                .in_flight
                .iter_mut()
                .find(|cell| cell.as_ref().is_some_and(|write| write.answered_by(&event)))
            else {
                return Phase::Ready(ready);
            };
            let write = cell.take().expect("the cell was found by its write");
            match advance(epoch, write, event, effects) {
                Outcome::Waiting(write) => *cell = Some(write),
                Outcome::Committed(write) => {
                    ready.view.add(&write.commits, effects);
                    for commit in write.commits {
                        effects.push(Effect::Ack {
                            request_id: commit.request_id,
                            sequence: commit.sequence,
                        });
                    }
                }
                Outcome::OlderEpoch(write, found) => {
                    effects.push(Effect::Alarm(Alarm::OlderEpochAfterDataWrite {
                        slot: write.slot,
                        found: found.epoch,
                    }));
                    return Phase::Stopped;
                }
                Outcome::NewerEpoch => return stop(effects),
                Outcome::OtherContent(write) => return other_content(write, effects),
            }
            Phase::Ready(ready)
        }
        (phase, _) => phase,
    }
}

fn load<A: Admit>(
    epoch: Epoch,
    admit: &mut A,
    slots: &BTreeMap<Slot, Segment>,
    waiting: Vec<Request>,
    effects: &mut Vec<Effect<A::Rejection>>,
) -> Phase {
    let last = slots.last_key_value();
    if !cfg!(feature = "no_epoch_check") && last.is_some_and(|(_, segment)| segment.epoch >= epoch)
    {
        return stop(effects);
    }
    let free = last.map_or(Slot(0), |(slot, _)| next(*slot));
    let view = replay(slots, admit, effects);
    if cfg!(feature = "no_fence") {
        return ready(view, free, waiting, admit, effects);
    }
    let fence = Write::new(epoch, free, Vec::new());
    effects.push(fence.put());
    Phase::Fencing {
        view,
        fence,
        waiting,
    }
}

fn replay<A: Admit>(
    slots: &BTreeMap<Slot, Segment>,
    admit: &mut A,
    effects: &mut Vec<Effect<A::Rejection>>,
) -> View {
    let mut view = View::default();
    for (expected, (slot, segment)) in (0..).zip(slots) {
        if slot.0 != expected {
            break;
        }
        view.add_older(&segment.commits, admit, effects);
    }
    view
}

fn ready<A: Admit>(
    view: View,
    next: Slot,
    waiting: Vec<Request>,
    admit: &mut A,
    effects: &mut Vec<Effect<A::Rejection>>,
) -> Phase {
    let mut ready = Ready {
        view,
        next,
        batch: Vec::new(),
        in_flight: [const { None }; MAX_IN_FLIGHT],
    };
    for request in waiting {
        ready.receive(request, admit, effects);
    }
    Phase::Ready(ready)
}

fn stop<R>(effects: &mut Vec<Effect<R>>) -> Phase {
    effects.push(Effect::Stop);
    Phase::Stopped
}

fn other_content<R>(write: Write, effects: &mut Vec<Effect<R>>) -> Phase {
    effects.push(Effect::Alarm(Alarm::OtherContentInOwnEpoch {
        slot: write.slot,
    }));
    Phase::Stopped
}

fn next(slot: Slot) -> Slot {
    Slot(slot.0 + 1)
}

fn advance<R>(
    epoch: Epoch,
    mut write: Write,
    event: Event,
    effects: &mut Vec<Effect<R>>,
) -> Outcome {
    match event {
        Event::PutSucceeded { .. } => Outcome::Committed(write),
        Event::PutFailed { failure, .. } => {
            if cfg!(feature = "unknown_as_success") && failure == PutFailure::Unknown {
                return Outcome::Committed(write);
            }
            if cfg!(feature = "rejected_as_fenced") && failure == PutFailure::AlreadyExists {
                return Outcome::NewerEpoch;
            }
            write.awaiting = Awaiting::ReadBack;
            effects.push(Effect::Get { slot: write.slot });
            Outcome::Waiting(write)
        }
        Event::ReadBack { found: None, .. } => {
            write.attempt += 1;
            write.awaiting = Awaiting::Reply;
            effects.push(write.put());
            Outcome::Waiting(write)
        }
        Event::ReadBack {
            found: Some(found), ..
        } => match found.epoch.cmp(&epoch) {
            Ordering::Equal if found.commits == write.commits => Outcome::Committed(write),
            Ordering::Equal => Outcome::OtherContent(write),
            Ordering::Greater => Outcome::NewerEpoch,
            Ordering::Less => Outcome::OlderEpoch(write, found),
        },
        Event::Loaded { .. } | Event::Request(_) | Event::Flush => Outcome::Waiting(write),
    }
}

impl Ready {
    fn receive<A: Admit>(
        &mut self,
        request: Request,
        admit: &mut A,
        effects: &mut Vec<Effect<A::Rejection>>,
    ) {
        let request_id = &request.request_id;
        if !cfg!(feature = "no_deduplication")
            && let Some(&sequence) = self.view.sequences.get(request_id)
        {
            effects.push(Effect::Ack {
                request_id: request.request_id,
                sequence,
            });
            return;
        }
        let in_batch = self.batch.iter().any(|held| held.request_id == *request_id);
        let in_put = !cfg!(feature = "no_inflight_deduplication")
            && self
                .in_flight
                .iter()
                .flatten()
                .flat_map(|write| &write.commits)
                .any(|commit| commit.request_id == *request_id);
        if in_batch || in_put {
            return;
        }
        match admit.admit(&request.payload) {
            Ok(()) => self.batch.push(request),
            Err(reason) => effects.push(Effect::Rejected {
                request_id: request.request_id,
                reason,
            }),
        }
    }

    fn flush<R>(&mut self, epoch: Epoch, effects: &mut Vec<Effect<R>>) {
        if self.batch.is_empty() {
            return;
        }
        let Some(cell) = self.in_flight.iter_mut().find(|cell| cell.is_none()) else {
            return;
        };
        let commits = self
            .batch
            .drain(..)
            .zip(self.view.next_sequence.0..)
            .map(|(request, sequence)| Commit {
                request_id: request.request_id,
                sequence: Sequence(sequence),
                payload: request.payload,
            })
            .collect();
        let write = Write::new(epoch, self.next, commits);
        effects.push(write.put());
        *cell = Some(write);
        self.next = next(self.next);
    }
}

impl View {
    fn add_older<A: Admit>(
        &mut self,
        commits: &[Commit],
        admit: &mut A,
        effects: &mut Vec<Effect<A::Rejection>>,
    ) {
        self.add(commits, effects);
        for commit in commits {
            admit.replay(&commit.payload);
        }
    }

    fn add<R>(&mut self, commits: &[Commit], effects: &mut Vec<Effect<R>>) {
        for commit in commits {
            self.sequences
                .insert(commit.request_id.clone(), commit.sequence);
            self.next_sequence = self.next_sequence.next();
            effects.push(Effect::Committed(commit.clone()));
        }
    }
}

impl Write {
    fn new(epoch: Epoch, slot: Slot, commits: Vec<Commit>) -> Write {
        Write {
            slot,
            body: segment::encode(epoch, &commits),
            commits,
            attempt: 0,
            awaiting: Awaiting::Reply,
        }
    }

    fn put<R>(&self) -> Effect<R> {
        Effect::Put {
            slot: self.slot,
            body: self.body.clone(),
            attempt: self.attempt,
        }
    }

    fn answered_by(&self, event: &Event) -> bool {
        let (slot, awaiting) = match event {
            Event::PutSucceeded { slot } | Event::PutFailed { slot, .. } => (slot, Awaiting::Reply),
            Event::ReadBack { slot, .. } => (slot, Awaiting::ReadBack),
            Event::Loaded { .. } | Event::Request(_) | Event::Flush => return false,
        };
        *slot == self.slot && awaiting == self.awaiting
    }
}

#[cfg(test)]
mod replay;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    type Effect = super::Effect<Infallible>;

    impl<A: Admit> Owner<A> {
        fn step_and_flush(&mut self, event: Event) -> Vec<super::Effect<A::Rejection>> {
            let mut effects = self.step(event);
            effects.extend(self.step(Event::Flush));
            effects
        }
    }

    fn id(name: &'static str) -> RequestId {
        RequestId(Bytes::from_static(name.as_bytes()))
    }

    fn request(name: &'static str) -> Event {
        Event::Request(Request {
            request_id: id(name),
            payload: Bytes::from_static(b"payload"),
        })
    }

    fn commit(name: &'static str, sequence: u64) -> Commit {
        Commit {
            request_id: id(name),
            sequence: Sequence(sequence),
            payload: Bytes::from_static(b"payload"),
        }
    }

    fn committed(name: &'static str, sequence: u64) -> Effect {
        Effect::Committed(commit(name, sequence))
    }

    fn ack(name: &'static str, sequence: u64) -> Effect {
        Effect::Ack {
            request_id: id(name),
            sequence: Sequence(sequence),
        }
    }

    fn put<R: std::fmt::Debug>(effects: &[super::Effect<R>]) -> (Slot, Segment, u32) {
        let puts: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                super::Effect::Put {
                    slot,
                    body,
                    attempt,
                } => Some((
                    *slot,
                    Segment::decode(body.clone()).expect("own body"),
                    *attempt,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(puts.len(), 1, "one put in {effects:?}");
        puts.into_iter().next().expect("one put")
    }

    fn acks<R>(effects: &[super::Effect<R>]) -> Vec<RequestId> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                super::Effect::Ack { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn request_ids(segment: &Segment) -> Vec<RequestId> {
        segment
            .commits
            .iter()
            .map(|commit| commit.request_id.clone())
            .collect()
    }

    #[derive(Default)]
    struct Store(BTreeMap<Slot, Segment>);

    impl Store {
        fn put(&mut self, slot: Slot, segment: Segment) -> Event {
            if self.0.contains_key(&slot) {
                return Event::PutFailed {
                    slot,
                    failure: PutFailure::AlreadyExists,
                };
            }
            self.0.insert(slot, segment);
            Event::PutSucceeded { slot }
        }

        fn read_back(&self, slot: Slot) -> Event {
            Event::ReadBack {
                slot,
                found: self.0.get(&slot).cloned(),
            }
        }

        fn loaded(&self) -> Event {
            Event::Loaded {
                slots: self.0.clone(),
            }
        }

        fn replay(&self) -> Vec<RequestId> {
            self.0.values().flat_map(request_ids).collect()
        }
    }

    fn ready_owner<A: Admit<Rejection: std::fmt::Debug>>(
        store: &mut Store,
        epoch: u64,
        admit: A,
    ) -> Owner<A> {
        let mut owner = Owner::new(Epoch(epoch), admit);
        let (slot, fence, _) = put(&owner.step_and_flush(store.loaded()));
        assert!(fence.commits.is_empty());
        assert!(owner.step_and_flush(store.put(slot, fence)).is_empty());
        owner
    }

    #[test]
    fn a_new_owner_fences_the_next_slot_and_then_writes_after_it() {
        let mut store = Store::default();
        let mut owner = Owner::new(Epoch(1), AdmitAll);
        assert_eq!(
            put(&owner.step_and_flush(store.loaded())),
            (
                Slot(0),
                Segment {
                    epoch: Epoch(1),
                    commits: vec![]
                },
                0
            )
        );
        assert_eq!(
            owner.step_and_flush(store.put(
                Slot(0),
                Segment {
                    epoch: Epoch(1),
                    commits: vec![],
                },
            )),
            vec![]
        );
        let (slot, segment, _) = put(&owner.step_and_flush(request("r1")));
        assert_eq!(slot, Slot(1));
        assert_eq!(segment.epoch, Epoch(1));
        assert_eq!(segment.commits[0].sequence, Sequence(0));
        assert_eq!(
            acks(&owner.step_and_flush(store.put(slot, segment))),
            vec![id("r1")]
        );
    }

    #[test]
    fn requests_that_arrive_during_a_put_form_the_next_batch() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 1, AdmitAll);
        let (first_slot, first, _) = put(&owner.step_and_flush(request("r1")));
        assert_eq!(owner.step_and_flush(request("r2")), vec![]);
        assert_eq!(owner.step_and_flush(request("r3")), vec![]);
        let effects = owner.step_and_flush(store.put(first_slot, first));
        assert_eq!(acks(&effects), vec![id("r1")]);
        let (slot, batch, _) = put(&effects);
        assert_eq!(slot, Slot(2));
        assert_eq!(request_ids(&batch), vec![id("r2"), id("r3")]);
        assert_eq!(
            batch
                .commits
                .iter()
                .map(|commit| commit.sequence)
                .collect::<Vec<_>>(),
            vec![Sequence(1), Sequence(2)]
        );
    }

    #[test]
    fn a_request_already_held_is_not_written_again() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 1, AdmitAll);
        let (slot, segment, _) = put(&owner.step_and_flush(request("r1")));
        assert_eq!(
            owner.step_and_flush(request("r1")),
            vec![],
            "in the put in flight"
        );
        assert_eq!(owner.step_and_flush(request("r2")), vec![]);
        assert_eq!(owner.step_and_flush(request("r2")), vec![], "in the batch");
        let effects = owner.step_and_flush(store.put(slot, segment));
        assert_eq!(request_ids(&put(&effects).1), vec![id("r2")]);
        assert_eq!(
            owner.step_and_flush(request("r1")),
            vec![ack("r1", 0)],
            "in the view"
        );
    }

    #[derive(Clone, Default)]
    struct Checked {
        admitted: Rc<RefCell<Vec<Bytes>>>,
        replayed: Rc<RefCell<Vec<Bytes>>>,
    }

    impl Admit for Checked {
        type Rejection = &'static str;

        fn admit(&mut self, payload: &Bytes) -> Result<(), &'static str> {
            self.admitted.borrow_mut().push(payload.clone());
            if payload.as_ref() == b"bad" {
                return Err("bad");
            }
            Ok(())
        }

        fn replay(&mut self, payload: &Bytes) {
            self.replayed.borrow_mut().push(payload.clone());
        }
    }

    impl Checked {
        fn admitted(&self) -> Vec<Bytes> {
            self.admitted.borrow().clone()
        }

        fn replayed(&self) -> Vec<Bytes> {
            self.replayed.borrow().clone()
        }
    }

    fn carrying(name: &'static str, payload: &'static str) -> Request {
        Request {
            request_id: id(name),
            payload: Bytes::from_static(payload.as_bytes()),
        }
    }

    fn committed_as(name: &'static str, sequence: u64, payload: &'static str) -> Commit {
        Commit {
            request_id: id(name),
            sequence: Sequence(sequence),
            payload: Bytes::from_static(payload.as_bytes()),
        }
    }

    #[test]
    fn only_a_new_request_is_checked_and_a_rejected_one_takes_no_sequence() {
        let mut store = Store::default();
        let checked = Checked::default();
        let mut owner = ready_owner(&mut store, 1, checked.clone());
        let (slot, segment, _) = put(&owner.step_and_flush(Event::Request(carrying("r1", "good"))));
        assert_eq!(
            owner.step_and_flush(Event::Request(carrying("r2", "bad"))),
            vec![super::Effect::Rejected {
                request_id: id("r2"),
                reason: "bad"
            }]
        );
        assert_eq!(
            owner.step_and_flush(Event::Request(carrying("r1", "bad"))),
            vec![],
            "a request in the put in flight is not checked again"
        );
        assert_eq!(
            acks(&owner.step_and_flush(store.put(slot, segment))),
            vec![id("r1")]
        );
        let (_, segment, _) = put(&owner.step_and_flush(Event::Request(carrying("r3", "good"))));
        assert_eq!(
            segment.commits,
            vec![committed_as("r3", 1, "good")],
            "the rejected request took no Commit Sequence"
        );
        assert_eq!(checked.admitted(), ["good", "bad", "good"]);
    }

    #[test]
    fn a_new_owner_replays_the_commits_of_older_owners_in_sequence_order() {
        let mut store = Store::default();
        store.put(
            Slot(0),
            Segment {
                epoch: Epoch(1),
                commits: vec![committed_as("r1", 0, "first"), committed_as("r2", 1, "bad")],
            },
        );
        let checked = Checked::default();
        let mut owner = Owner::new(Epoch(3), checked.clone());
        let (fence_slot, fence, _) = put(&owner.step_and_flush(store.loaded()));
        store.put(
            fence_slot,
            Segment {
                epoch: Epoch(2),
                commits: vec![committed_as("r3", 2, "third")],
            },
        );
        assert_eq!(
            owner.step_and_flush(store.put(fence_slot, fence)),
            vec![super::Effect::Get { slot: fence_slot }]
        );
        let effects = owner.step_and_flush(store.read_back(fence_slot));
        assert_eq!(
            effects[0],
            super::Effect::Committed(committed_as("r3", 2, "third"))
        );
        assert_eq!(
            checked.replayed(),
            ["first", "bad", "third"],
            "every commit, a bad one too"
        );
        assert!(
            checked.admitted().is_empty(),
            "no replayed commit is checked"
        );
    }

    #[test]
    fn requests_before_the_fence_succeeds_wait_for_it() {
        let mut store = Store::default();
        store.put(
            Slot(0),
            Segment {
                epoch: Epoch(1),
                commits: vec![Commit {
                    request_id: id("r1"),
                    sequence: Sequence(0),
                    payload: Bytes::new(),
                }],
            },
        );
        let mut owner = Owner::new(Epoch(2), AdmitAll);
        assert_eq!(owner.step_and_flush(request("r1")), vec![]);
        let (slot, fence, _) = put(&owner.step_and_flush(store.loaded()));
        assert_eq!(owner.step_and_flush(request("r2")), vec![]);
        let effects = owner.step_and_flush(store.put(slot, fence));
        assert_eq!(acks(&effects), vec![id("r1")]);
        let (slot, segment, _) = put(&effects);
        assert_eq!((slot, request_ids(&segment)), (Slot(2), vec![id("r2")]));
        assert_eq!(segment.commits[0].sequence, Sequence(1));
    }

    #[test]
    fn an_owner_stops_when_the_last_slot_has_its_epoch_or_a_newer_one() {
        let mut store = Store::default();
        store.put(
            Slot(0),
            Segment {
                epoch: Epoch(1),
                commits: vec![],
            },
        );
        store.put(
            Slot(1),
            Segment {
                epoch: Epoch(3),
                commits: vec![],
            },
        );
        assert_eq!(
            Owner::new(Epoch(2), AdmitAll).step_and_flush(store.loaded()),
            vec![Effect::Stop]
        );
        assert_eq!(
            Owner::new(Epoch(3), AdmitAll).step_and_flush(store.loaded()),
            vec![Effect::Stop],
            "a reload takes a new epoch"
        );
        let (slot, _, _) = put(&Owner::new(Epoch(4), AdmitAll).step_and_flush(store.loaded()));
        assert_eq!(slot, Slot(2));
    }

    #[test]
    fn a_read_back_decides_by_the_epoch_and_checks_the_content() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 2, AdmitAll);
        let (slot, written, _) = put(&owner.step_and_flush(request("r1")));
        let failed = |slot| Event::PutFailed {
            slot,
            failure: PutFailure::AlreadyExists,
        };
        assert_eq!(
            owner.step_and_flush(failed(slot)),
            vec![Effect::Get { slot }]
        );
        assert_eq!(
            owner.step_and_flush(Event::ReadBack {
                slot,
                found: Some(written)
            }),
            vec![committed("r1", 0), ack("r1", 0)]
        );

        let (slot, _, _) = put(&owner.step_and_flush(request("r2")));
        assert_eq!(
            owner.step_and_flush(failed(slot)),
            vec![Effect::Get { slot }]
        );
        let newer = Segment {
            epoch: Epoch(3),
            commits: vec![],
        };
        assert_eq!(
            owner.step_and_flush(Event::ReadBack {
                slot,
                found: Some(newer)
            }),
            vec![Effect::Stop]
        );
        assert_eq!(
            owner.step_and_flush(request("r3")),
            vec![],
            "a stopped owner does nothing"
        );
    }

    #[test]
    fn other_content_under_the_own_epoch_raises_an_alarm() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 2, AdmitAll);
        let (slot, _, _) = put(&owner.step_and_flush(request("r1")));
        assert_eq!(
            owner.step_and_flush(Event::PutFailed {
                slot,
                failure: PutFailure::AlreadyExists,
            }),
            vec![Effect::Get { slot }]
        );
        let other = Segment {
            epoch: Epoch(2),
            commits: vec![],
        };
        assert_eq!(
            owner.step_and_flush(Event::ReadBack {
                slot,
                found: Some(other)
            }),
            vec![Effect::Alarm(Alarm::OtherContentInOwnEpoch { slot })]
        );

        let mut fencing = Owner::new(Epoch(3), AdmitAll);
        let (slot, _, _) = put(&fencing.step_and_flush(store.loaded()));
        assert_eq!(
            fencing.step_and_flush(Event::PutFailed {
                slot,
                failure: PutFailure::AlreadyExists,
            }),
            vec![Effect::Get { slot }]
        );
        let earlier_write = Segment {
            epoch: Epoch(3),
            commits: vec![Commit {
                request_id: id("r1"),
                sequence: Sequence(0),
                payload: Bytes::from_static(b"payload"),
            }],
        };
        assert_eq!(
            fencing.step_and_flush(Event::ReadBack {
                slot,
                found: Some(earlier_write)
            }),
            vec![Effect::Alarm(Alarm::OtherContentInOwnEpoch { slot })],
            "a fence slot that holds commits of the own epoch"
        );
    }

    #[test]
    fn a_data_write_that_finds_an_older_epoch_raises_an_alarm() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 2, AdmitAll);
        let (slot, _, _) = put(&owner.step_and_flush(request("r1")));
        assert_eq!(
            owner.step_and_flush(Event::PutFailed {
                slot,
                failure: PutFailure::Unknown,
            }),
            vec![Effect::Get { slot }]
        );
        let older = Segment {
            epoch: Epoch(1),
            commits: vec![],
        };
        assert_eq!(
            owner.step_and_flush(Event::ReadBack {
                slot,
                found: Some(older)
            }),
            vec![Effect::Alarm(Alarm::OlderEpochAfterDataWrite {
                slot,
                found: Epoch(1)
            })]
        );
    }

    #[test]
    fn an_empty_read_back_resends_the_same_bytes() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 1, AdmitAll);
        let effects = owner.step_and_flush(request("r1"));
        let Effect::Put {
            slot,
            body,
            attempt: 0,
        } = effects[0].clone()
        else {
            panic!("{effects:?}")
        };
        for attempt in 1..3 {
            assert_eq!(
                owner.step_and_flush(Event::PutFailed {
                    slot,
                    failure: PutFailure::Unknown,
                }),
                vec![Effect::Get { slot }]
            );
            assert_eq!(
                owner.step_and_flush(Event::ReadBack { slot, found: None }),
                vec![Effect::Put {
                    slot,
                    body: body.clone(),
                    attempt
                }]
            );
        }
    }

    #[test]
    fn events_for_other_slots_or_stages_change_nothing() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 1, AdmitAll);
        let (slot, _, _) = put(&owner.step_and_flush(request("r1")));
        assert_eq!(
            owner.step_and_flush(Event::ReadBack { slot, found: None }),
            vec![],
            "no read-back was asked"
        );
        assert_eq!(
            owner.step_and_flush(Event::PutSucceeded { slot: Slot(9) }),
            vec![]
        );
        assert_eq!(owner.step_and_flush(store.loaded()), vec![]);
        assert_eq!(
            acks(&owner.step_and_flush(Event::PutSucceeded { slot })),
            vec![id("r1")]
        );

        let mut fencing = Owner::new(Epoch(2), AdmitAll);
        let (slot, _, _) = put(&fencing.step_and_flush(store.loaded()));
        assert_eq!(
            fencing.step_and_flush(Event::ReadBack { slot, found: None }),
            vec![]
        );
        assert_eq!(
            fencing.step_and_flush(Event::PutSucceeded { slot: Slot(9) }),
            vec![]
        );
        assert_eq!(
            fencing.step_and_flush(Event::PutSucceeded { slot }),
            vec![],
            "now ready"
        );
        let (next, _, _) = put(&fencing.step_and_flush(request("r2")));
        assert_eq!(next, Slot(slot.0 + 1));
    }

    #[test]
    fn old_owner_takes_slot_first() {
        let mut store = Store::default();
        let mut a = ready_owner(&mut store, 1, AdmitAll);
        let (a_slot, a_write, _) = put(&a.step_and_flush(request("r1")));
        let mut b = Owner::new(Epoch(2), AdmitAll);
        let (b_slot, b_fence, _) = put(&b.step_and_flush(store.loaded()));
        assert_eq!(b_slot, Slot(1));
        let a_reply = store.put(a_slot, a_write);
        let b_reply = store.put(b_slot, b_fence);
        assert_eq!(acks(&a.step_and_flush(a_reply)), vec![id("r1")]);
        assert_eq!(
            b.step_and_flush(b_reply),
            vec![Effect::Get { slot: b_slot }]
        );
        let (slot, fence, _) = put(&b.step_and_flush(store.read_back(b_slot)));
        assert_eq!(slot, Slot(2));
        assert_eq!(b.step_and_flush(store.put(slot, fence)), vec![]);
        assert_eq!(
            b.step_and_flush(request("r1")),
            vec![ack("r1", 0)],
            "B sees r1"
        );
        assert_eq!(store.replay(), vec![id("r1")]);
        assert_eq!(
            store.0[&Slot(2)],
            Segment {
                epoch: Epoch(2),
                commits: vec![]
            }
        );

        let (slot, write, _) = put(&a.step_and_flush(request("r2")));
        assert_eq!(
            a.step_and_flush(store.put(slot, write)),
            vec![Effect::Get { slot }]
        );
        assert_eq!(
            a.step_and_flush(store.read_back(slot)),
            vec![Effect::Stop],
            "A stops at the newer epoch"
        );
        let (slot, segment, _) = put(&b.step_and_flush(request("r2")));
        assert_eq!(segment.commits[0].sequence, Sequence(1));
        assert_eq!(
            acks(&b.step_and_flush(store.put(slot, segment))),
            vec![id("r2")]
        );
    }

    #[test]
    fn own_resend_acknowledged() {
        let mut store = Store::default();
        let mut owner = ready_owner(&mut store, 1, AdmitAll);
        let (slot, first, 0) = put(&owner.step_and_flush(request("r1"))) else {
            panic!("first attempt")
        };
        assert_eq!(
            owner.step_and_flush(Event::PutFailed {
                slot,
                failure: PutFailure::Unknown
            }),
            vec![Effect::Get { slot }]
        );
        let (_, resend, 1) = put(&owner.step_and_flush(store.read_back(slot))) else {
            panic!("second attempt")
        };
        assert_eq!(resend, first);
        store.put(slot, first);
        assert_eq!(
            owner.step_and_flush(store.put(slot, resend)),
            vec![Effect::Get { slot }]
        );
        assert_eq!(
            owner.step_and_flush(store.read_back(slot)),
            vec![committed("r1", 0), ack("r1", 0)]
        );
        assert_eq!(store.replay(), vec![id("r1")]);
    }

    #[test]
    fn every_commit_is_passed_on_once_in_sequence_order_before_its_ack() {
        let mut store = Store::default();
        store.put(
            Slot(0),
            Segment {
                epoch: Epoch(1),
                commits: vec![commit("r1", 0)],
            },
        );
        let mut owner = Owner::new(Epoch(3), AdmitAll);
        let effects = owner.step_and_flush(store.loaded());
        assert_eq!(effects[0], committed("r1", 0), "read on load");
        let (fence_slot, fence, _) = put(&effects);
        store.put(
            fence_slot,
            Segment {
                epoch: Epoch(2),
                commits: vec![commit("r2", 1)],
            },
        );
        assert_eq!(
            owner.step_and_flush(store.put(fence_slot, fence)),
            vec![Effect::Get { slot: fence_slot }]
        );
        let effects = owner.step_and_flush(store.read_back(fence_slot));
        assert_eq!(effects[0], committed("r2", 1), "found by the fence");
        let (slot, fence, _) = put(&effects);
        assert_eq!(owner.step_and_flush(store.put(slot, fence)), vec![]);

        let (slot, segment, _) = put(&owner.step_and_flush(request("r3")));
        assert_eq!(
            owner.step_and_flush(store.put(slot, segment)),
            vec![committed("r3", 2), ack("r3", 2)],
            "own write"
        );
        assert_eq!(
            owner.step_and_flush(request("r2")),
            vec![ack("r2", 1)],
            "a resend is acknowledged with its first sequence"
        );
    }
}
