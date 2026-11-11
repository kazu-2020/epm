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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Start {
    pub slot: Slot,
    pub request_ids: Vec<RequestId>,
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
    Committed {
        slot: Slot,
        commit: Commit,
    },
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
    StartNotInLog { slot: Slot, sequence: Sequence },
}

pub trait Admit {
    type Rejection;

    fn admit(&mut self, payload: &Bytes) -> Result<(), Self::Rejection>;

    fn replay(&mut self, payload: &Bytes);

    fn ready(&mut self, _next: Sequence) {}
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
        start: Start,
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
        Owner::resume(epoch, admit, Start::default())
    }

    pub fn resume(epoch: Epoch, admit: A, start: Start) -> Owner<A> {
        Owner {
            epoch,
            admit,
            phase: Phase::Loading {
                start,
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
        (Phase::Loading { start, waiting }, Event::Loaded { slots }) => {
            load(epoch, admit, start, &slots, waiting, effects)
        }
        (Phase::Loading { start, mut waiting }, Event::Request(request)) => {
            waiting.push(request);
            Phase::Loading { start, waiting }
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
                view.add_older(&found.commits, taken.slot, admit, effects);
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
                    ready.view.add(&write.commits, write.slot, effects);
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
    start: Start,
    slots: &BTreeMap<Slot, Segment>,
    waiting: Vec<Request>,
    effects: &mut Vec<Effect<A::Rejection>>,
) -> Phase {
    let last = slots.last_key_value();
    if !cfg!(feature = "no_epoch_check") && last.is_some_and(|(_, segment)| segment.epoch >= epoch)
    {
        return stop(effects);
    }
    let free = last.map_or(start.slot, |(slot, _)| next(*slot));
    if let Some((sequence, request_id)) = start.last() {
        let found = slots.get(&start.slot).is_some_and(|segment| {
            segment
                .commits
                .iter()
                .any(|commit| commit.sequence == sequence && commit.request_id == *request_id)
        });
        if !found {
            effects.push(Effect::Alarm(Alarm::StartNotInLog {
                slot: start.slot,
                sequence,
            }));
            return Phase::Stopped;
        }
    }
    let first = start.slot;
    let view = replay(View::from(start), first, slots, admit, effects);
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
    mut view: View,
    first: Slot,
    slots: &BTreeMap<Slot, Segment>,
    admit: &mut A,
    effects: &mut Vec<Effect<A::Rejection>>,
) -> View {
    let mut expected = first;
    for (slot, segment) in slots.range(first..) {
        if *slot != expected {
            break;
        }
        expected = next(*slot);
        // Not every commit of the first slot: the start already counts the commits before its next sequence, and that slot can hold some of them.
        let skipped = if *slot == first {
            segment
                .commits
                .partition_point(|commit| commit.sequence < view.next_sequence)
        } else {
            0
        };
        view.add_older(&segment.commits[skipped..], *slot, admit, effects);
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
    admit.ready(ready.view.next_sequence);
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
            Err(reason) => {
                effects.push(Effect::Rejected {
                    request_id: request.request_id.clone(),
                    reason,
                });
                if cfg!(feature = "commit_rejected") {
                    self.batch.push(request);
                }
            }
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

impl Start {
    fn last(&self) -> Option<(Sequence, &RequestId)> {
        let count = u64::try_from(self.request_ids.len()).expect("a count fits in u64");
        Some((Sequence(count.checked_sub(1)?), self.request_ids.last()?))
    }
}

impl From<Start> for View {
    fn from(start: Start) -> View {
        let next_sequence = start
            .last()
            .map_or(Sequence(0), |(sequence, _)| sequence.next());
        let sequences = (0..)
            .map(Sequence)
            .zip(start.request_ids)
            .map(|(sequence, request_id)| (request_id, sequence))
            .collect();
        View {
            sequences,
            next_sequence,
        }
    }
}

impl View {
    fn add_older<A: Admit>(
        &mut self,
        commits: &[Commit],
        slot: Slot,
        admit: &mut A,
        effects: &mut Vec<Effect<A::Rejection>>,
    ) {
        self.add(commits, slot, effects);
        for commit in commits {
            admit.replay(&commit.payload);
        }
    }

    fn add<R>(&mut self, commits: &[Commit], slot: Slot, effects: &mut Vec<Effect<R>>) {
        for commit in commits {
            self.sequences
                .insert(commit.request_id.clone(), commit.sequence);
            self.next_sequence = self.next_sequence.next();
            effects.push(Effect::Committed {
                slot,
                commit: commit.clone(),
            });
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
mod tests;
