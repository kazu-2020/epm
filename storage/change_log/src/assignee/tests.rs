use std::cell::RefCell;
use std::rc::Rc;

use super::*;

type Effect = super::Effect<Infallible>;

impl<A: Admit> Assignee<A> {
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

fn committed(name: &'static str, sequence: u64, slot: Slot) -> Effect {
    Effect::Committed {
        slot,
        commit: commit(name, sequence),
    }
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

fn ready_assignee<A: Admit<Rejection: std::fmt::Debug>>(
    store: &mut Store,
    epoch: u64,
    admit: A,
) -> Assignee<A> {
    let mut assignee = Assignee::new(Epoch(epoch), admit);
    let (slot, fence, _) = put(&assignee.step_and_flush(store.loaded()));
    assert!(fence.commits.is_empty());
    assert!(assignee.step_and_flush(store.put(slot, fence)).is_empty());
    assignee
}

#[test]
fn a_new_assignee_fences_the_next_slot_and_then_writes_after_it() {
    let mut store = Store::default();
    let mut assignee = Assignee::new(Epoch(1), AdmitAll);
    assert_eq!(
        put(&assignee.step_and_flush(store.loaded())),
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
        assignee.step_and_flush(store.put(
            Slot(0),
            Segment {
                epoch: Epoch(1),
                commits: vec![],
            },
        )),
        vec![]
    );
    let (slot, segment, _) = put(&assignee.step_and_flush(request("r1")));
    assert_eq!(slot, Slot(1));
    assert_eq!(segment.epoch, Epoch(1));
    assert_eq!(segment.commits[0].sequence, Sequence(0));
    assert_eq!(
        acks(&assignee.step_and_flush(store.put(slot, segment))),
        vec![id("r1")]
    );
}

#[test]
fn requests_that_arrive_during_a_put_form_the_next_batch() {
    let mut store = Store::default();
    let mut assignee = ready_assignee(&mut store, 1, AdmitAll);
    let (first_slot, first, _) = put(&assignee.step_and_flush(request("r1")));
    assert_eq!(assignee.step_and_flush(request("r2")), vec![]);
    assert_eq!(assignee.step_and_flush(request("r3")), vec![]);
    let effects = assignee.step_and_flush(store.put(first_slot, first));
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
    let mut assignee = ready_assignee(&mut store, 1, AdmitAll);
    let (slot, segment, _) = put(&assignee.step_and_flush(request("r1")));
    assert_eq!(
        assignee.step_and_flush(request("r1")),
        vec![],
        "in the put in flight"
    );
    assert_eq!(assignee.step_and_flush(request("r2")), vec![]);
    assert_eq!(
        assignee.step_and_flush(request("r2")),
        vec![],
        "in the batch"
    );
    let effects = assignee.step_and_flush(store.put(slot, segment));
    assert_eq!(request_ids(&put(&effects).1), vec![id("r2")]);
    assert_eq!(
        assignee.step_and_flush(request("r1")),
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
    let mut assignee = ready_assignee(&mut store, 1, checked.clone());
    let (slot, segment, _) = put(&assignee.step_and_flush(Event::Request(carrying("r1", "good"))));
    assert_eq!(
        assignee.step_and_flush(Event::Request(carrying("r2", "bad"))),
        vec![super::Effect::Rejected {
            request_id: id("r2"),
            reason: "bad"
        }]
    );
    assert_eq!(
        assignee.step_and_flush(Event::Request(carrying("r1", "bad"))),
        vec![],
        "a request in the put in flight is not checked again"
    );
    assert_eq!(
        acks(&assignee.step_and_flush(store.put(slot, segment))),
        vec![id("r1")]
    );
    let (_, segment, _) = put(&assignee.step_and_flush(Event::Request(carrying("r3", "good"))));
    assert_eq!(
        segment.commits,
        vec![committed_as("r3", 1, "good")],
        "the rejected request took no Commit Sequence"
    );
    assert_eq!(checked.admitted(), ["good", "bad", "good"]);
}

#[test]
fn a_new_assignee_replays_the_commits_of_older_assignees_in_sequence_order() {
    let mut store = Store::default();
    store.put(
        Slot(0),
        Segment {
            epoch: Epoch(1),
            commits: vec![committed_as("r1", 0, "first"), committed_as("r2", 1, "bad")],
        },
    );
    let checked = Checked::default();
    let mut assignee = Assignee::new(Epoch(3), checked.clone());
    let (fence_slot, fence, _) = put(&assignee.step_and_flush(store.loaded()));
    store.put(
        fence_slot,
        Segment {
            epoch: Epoch(2),
            commits: vec![committed_as("r3", 2, "third")],
        },
    );
    assert_eq!(
        assignee.step_and_flush(store.put(fence_slot, fence)),
        vec![super::Effect::Get { slot: fence_slot }]
    );
    let effects = assignee.step_and_flush(store.read_back(fence_slot));
    assert_eq!(
        effects[0],
        super::Effect::Committed {
            slot: fence_slot,
            commit: committed_as("r3", 2, "third")
        }
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
    let mut assignee = Assignee::new(Epoch(2), AdmitAll);
    assert_eq!(assignee.step_and_flush(request("r1")), vec![]);
    let (slot, fence, _) = put(&assignee.step_and_flush(store.loaded()));
    assert_eq!(assignee.step_and_flush(request("r2")), vec![]);
    let effects = assignee.step_and_flush(store.put(slot, fence));
    assert_eq!(acks(&effects), vec![id("r1")]);
    let (slot, segment, _) = put(&effects);
    assert_eq!((slot, request_ids(&segment)), (Slot(2), vec![id("r2")]));
    assert_eq!(segment.commits[0].sequence, Sequence(1));
}

#[test]
fn an_assignee_stops_when_the_last_slot_has_its_epoch_or_a_newer_one() {
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
        Assignee::new(Epoch(2), AdmitAll).step_and_flush(store.loaded()),
        vec![Effect::Stop]
    );
    assert_eq!(
        Assignee::new(Epoch(3), AdmitAll).step_and_flush(store.loaded()),
        vec![Effect::Stop],
        "a reload takes a new epoch"
    );
    let (slot, _, _) = put(&Assignee::new(Epoch(4), AdmitAll).step_and_flush(store.loaded()));
    assert_eq!(slot, Slot(2));
}

#[test]
fn a_read_back_decides_by_the_epoch_and_checks_the_content() {
    let mut store = Store::default();
    let mut assignee = ready_assignee(&mut store, 2, AdmitAll);
    let (slot, written, _) = put(&assignee.step_and_flush(request("r1")));
    let failed = |slot| Event::PutFailed {
        slot,
        failure: PutFailure::AlreadyExists,
    };
    assert_eq!(
        assignee.step_and_flush(failed(slot)),
        vec![Effect::Get { slot }]
    );
    assert_eq!(
        assignee.step_and_flush(Event::ReadBack {
            slot,
            found: Some(written)
        }),
        vec![committed("r1", 0, slot), ack("r1", 0)]
    );

    let (slot, _, _) = put(&assignee.step_and_flush(request("r2")));
    assert_eq!(
        assignee.step_and_flush(failed(slot)),
        vec![Effect::Get { slot }]
    );
    let newer = Segment {
        epoch: Epoch(3),
        commits: vec![],
    };
    assert_eq!(
        assignee.step_and_flush(Event::ReadBack {
            slot,
            found: Some(newer)
        }),
        vec![Effect::Stop]
    );
    assert_eq!(
        assignee.step_and_flush(request("r3")),
        vec![],
        "a stopped assignee does nothing"
    );
}

#[test]
fn other_content_under_the_own_epoch_raises_an_alarm() {
    let mut store = Store::default();
    let mut assignee = ready_assignee(&mut store, 2, AdmitAll);
    let (slot, _, _) = put(&assignee.step_and_flush(request("r1")));
    assert_eq!(
        assignee.step_and_flush(Event::PutFailed {
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
        assignee.step_and_flush(Event::ReadBack {
            slot,
            found: Some(other)
        }),
        vec![Effect::Alarm(Alarm::OtherContentInOwnEpoch { slot })]
    );

    let mut fencing = Assignee::new(Epoch(3), AdmitAll);
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
    let mut assignee = ready_assignee(&mut store, 2, AdmitAll);
    let (slot, _, _) = put(&assignee.step_and_flush(request("r1")));
    assert_eq!(
        assignee.step_and_flush(Event::PutFailed {
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
        assignee.step_and_flush(Event::ReadBack {
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
    let mut assignee = ready_assignee(&mut store, 1, AdmitAll);
    let effects = assignee.step_and_flush(request("r1"));
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
            assignee.step_and_flush(Event::PutFailed {
                slot,
                failure: PutFailure::Unknown,
            }),
            vec![Effect::Get { slot }]
        );
        assert_eq!(
            assignee.step_and_flush(Event::ReadBack { slot, found: None }),
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
    let mut assignee = ready_assignee(&mut store, 1, AdmitAll);
    let (slot, _, _) = put(&assignee.step_and_flush(request("r1")));
    assert_eq!(
        assignee.step_and_flush(Event::ReadBack { slot, found: None }),
        vec![],
        "no read-back was asked"
    );
    assert_eq!(
        assignee.step_and_flush(Event::PutSucceeded { slot: Slot(9) }),
        vec![]
    );
    assert_eq!(assignee.step_and_flush(store.loaded()), vec![]);
    assert_eq!(
        acks(&assignee.step_and_flush(Event::PutSucceeded { slot })),
        vec![id("r1")]
    );

    let mut fencing = Assignee::new(Epoch(2), AdmitAll);
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
fn old_assignee_takes_slot_first() {
    let mut store = Store::default();
    let mut a = ready_assignee(&mut store, 1, AdmitAll);
    let (a_slot, a_write, _) = put(&a.step_and_flush(request("r1")));
    let mut b = Assignee::new(Epoch(2), AdmitAll);
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
    let mut assignee = ready_assignee(&mut store, 1, AdmitAll);
    let (slot, first, 0) = put(&assignee.step_and_flush(request("r1"))) else {
        panic!("first attempt")
    };
    assert_eq!(
        assignee.step_and_flush(Event::PutFailed {
            slot,
            failure: PutFailure::Unknown
        }),
        vec![Effect::Get { slot }]
    );
    let (_, resend, 1) = put(&assignee.step_and_flush(store.read_back(slot))) else {
        panic!("second attempt")
    };
    assert_eq!(resend, first);
    store.put(slot, first);
    assert_eq!(
        assignee.step_and_flush(store.put(slot, resend)),
        vec![Effect::Get { slot }]
    );
    assert_eq!(
        assignee.step_and_flush(store.read_back(slot)),
        vec![committed("r1", 0, slot), ack("r1", 0)]
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
    let mut assignee = Assignee::new(Epoch(3), AdmitAll);
    let effects = assignee.step_and_flush(store.loaded());
    assert_eq!(effects[0], committed("r1", 0, Slot(0)), "read on load");
    let (fence_slot, fence, _) = put(&effects);
    store.put(
        fence_slot,
        Segment {
            epoch: Epoch(2),
            commits: vec![commit("r2", 1)],
        },
    );
    assert_eq!(
        assignee.step_and_flush(store.put(fence_slot, fence)),
        vec![Effect::Get { slot: fence_slot }]
    );
    let effects = assignee.step_and_flush(store.read_back(fence_slot));
    assert_eq!(
        effects[0],
        committed("r2", 1, fence_slot),
        "found by the fence"
    );
    let (slot, fence, _) = put(&effects);
    assert_eq!(assignee.step_and_flush(store.put(slot, fence)), vec![]);

    let (slot, segment, _) = put(&assignee.step_and_flush(request("r3")));
    assert_eq!(
        assignee.step_and_flush(store.put(slot, segment)),
        vec![committed("r3", 2, slot), ack("r3", 2)],
        "own write"
    );
    assert_eq!(
        assignee.step_and_flush(request("r2")),
        vec![ack("r2", 1)],
        "a resend is acknowledged with its first sequence"
    );
}

fn three_slots() -> Store {
    let segment = |commits| Segment {
        epoch: Epoch(1),
        commits,
    };
    Store(BTreeMap::from([
        (Slot(0), segment(vec![commit("r1", 0), commit("r2", 1)])),
        (Slot(1), segment(vec![commit("r3", 2), commit("r4", 3)])),
        (Slot(2), segment(vec![commit("r5", 4)])),
    ]))
}

#[test]
fn an_assignee_from_a_start_continues_as_an_assignee_that_reads_the_whole_log() {
    let store = three_slots();
    let whole = Assignee::new(Epoch(2), AdmitAll).step_and_flush(store.loaded());
    let start = Start {
        slot: Slot(1),
        request_ids: vec![id("r1"), id("r2"), id("r3")],
    };
    let mut resumed = Assignee::resume(Epoch(2), AdmitAll, start);
    let tail = Event::Loaded {
        slots: store.0.clone().split_off(&Slot(1)),
    };
    let effects = resumed.step_and_flush(tail);
    assert_eq!(
        effects,
        whole[3..],
        "the commits from sequence 3 and the same fence"
    );

    let (slot, fence, _) = put(&effects);
    let mut store = store;
    assert!(resumed.step_and_flush(store.put(slot, fence)).is_empty());
    assert_eq!(
        resumed.step_and_flush(request("r1")),
        [ack("r1", 0)],
        "a resend from before the start keeps its sequence"
    );
    let (_, segment, _) = put(&resumed.step_and_flush(request("r6")));
    assert_eq!(segment.commits, [commit("r6", 5)]);
}

#[test]
fn a_start_that_the_log_does_not_hold_is_an_alarm() {
    let store = three_slots();
    let start = Start {
        slot: Slot(0),
        request_ids: vec![id("r1"), id("r2"), id("r3")],
    };
    assert_eq!(
        Assignee::resume(Epoch(2), AdmitAll, start).step_and_flush(store.loaded()),
        [Effect::Alarm(Alarm::StartNotInLog {
            slot: Slot(0),
            sequence: Sequence(2)
        })]
    );
}

#[test]
fn a_start_with_another_request_id_for_its_last_commit_is_an_alarm() {
    let start = Start {
        slot: Slot(1),
        request_ids: vec![id("r1"), id("r2"), id("other")],
    };
    assert_eq!(
        Assignee::resume(Epoch(2), AdmitAll, start).step_and_flush(three_slots().loaded()),
        [Effect::Alarm(Alarm::StartNotInLog {
            slot: Slot(1),
            sequence: Sequence(2)
        })]
    );
}

#[test]
fn the_replay_starts_only_at_the_slot_of_the_start() {
    let mut store = three_slots();
    store.0.remove(&Slot(0));
    let effects = Assignee::new(Epoch(2), AdmitAll).step_and_flush(store.loaded());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Committed { .. })),
        "{effects:?}"
    );
}
