use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use change_log::{Alarm, Commit, Epoch, Exit, RequestId, Segment, Sequence, Slot};

use super::simulate::{Outcome, Record, Refusal};

pub(super) fn acknowledged(history: &[Record]) -> BTreeSet<String> {
    history
        .iter()
        .filter_map(|record| match record {
            Record::Acked { request, .. } => Some(request.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn slot_of(key: &str) -> Slot {
    let name = key.rsplit('/').next().expect("a key has a name");
    Slot(name.parse().expect("a slot key"))
}

pub(super) fn decode(body: &Bytes) -> Segment {
    Segment::decode(body.clone()).expect("the assignee writes segments it can read")
}

pub fn replay(store: &BTreeMap<String, Bytes>) -> (BTreeMap<Slot, Segment>, Vec<Commit>) {
    let segments: BTreeMap<Slot, Segment> = store
        .iter()
        .map(|(key, body)| (slot_of(key), decode(body)))
        .collect();
    let log = segments
        .iter()
        .zip(0..)
        .take_while(|((slot, _), expected)| slot.0 == *expected)
        .flat_map(|((_, segment), _)| segment.commits.iter().cloned())
        .collect();
    (segments, log)
}

pub type Violations = BTreeMap<&'static str, String>;

pub fn check(outcome: &Outcome) -> Violations {
    let mut violations = Violations::new();
    let mut violate = |invariant: &'static str, detail: String| {
        violations.entry(invariant).or_insert(detail);
    };
    let mut store = BTreeMap::new();
    let mut log: Vec<Commit> = Vec::new();
    let mut written: BTreeMap<(Slot, Epoch), Bytes> = BTreeMap::new();
    let mut before_probe = None;
    let mut acknowledged: Vec<(usize, &String, u64, Sequence)> = Vec::new();
    let mut epochs_that_pass_on: BTreeSet<u64> = BTreeSet::new();
    let mut passed_on: BTreeMap<u64, Vec<(usize, &Commit)>> = BTreeMap::new();
    let mut recalculated: BTreeMap<u64, Vec<(usize, Sequence)>> = BTreeMap::new();
    let mut invalid: Vec<(usize, &String)> = Vec::new();
    for (at, record) in outcome.history.iter().enumerate() {
        match record {
            Record::Granted {
                epoch,
                passes_on: true,
                ..
            } => {
                epochs_that_pass_on.insert(*epoch);
            }
            Record::Sent { key, body } => {
                let first = written
                    .entry((slot_of(key), decode(body).epoch))
                    .or_insert_with(|| body.clone());
                if first != body {
                    violate("oneContentPerSlotPerEpoch", format!("record {at}: {key}"));
                }
            }
            Record::Applied { key, body } => {
                if store
                    .insert(key.clone(), body.clone())
                    .is_some_and(|old| old != *body)
                {
                    violate("segmentsNeverChange", format!("record {at}: {key} changed"));
                }
                let (segments, next) = replay(&store);
                if !segments
                    .keys()
                    .zip(0..)
                    .all(|(slot, expected)| slot.0 == expected)
                {
                    violate("slotsContiguous", format!("record {at}: hole before {key}"));
                }
                let epochs: Vec<Epoch> = segments.values().map(|segment| segment.epoch).collect();
                if !epochs.is_sorted() {
                    violate(
                        "staleEpochNeverCommits",
                        format!("record {at}: epochs {epochs:?}"),
                    );
                }
                if !next.starts_with(&log) {
                    violate(
                        "refinesChangeLog",
                        format!("record {at}: {key} took commits out of the log"),
                    );
                }
                let distinct: BTreeSet<&RequestId> =
                    next.iter().map(|commit| &commit.request_id).collect();
                if distinct.len() != next.len() {
                    violate(
                        "refinesChangeLog",
                        format!("record {at}: {key} wrote a Request ID twice"),
                    );
                }
                if !next
                    .iter()
                    .zip(0..)
                    .all(|(commit, position)| commit.sequence == Sequence(position))
                {
                    violate("commitSequenceIsPosition", format!("record {at}: {key}"));
                }
                log = next;
            }
            Record::PassedOn { epoch, commit } => {
                passed_on.entry(*epoch).or_default().push((at, commit));
            }
            Record::Recalculated { epoch, sequence } => {
                recalculated
                    .entry(*epoch)
                    .or_default()
                    .push((at, *sequence));
            }
            Record::EngineAlarmed {
                server,
                epoch,
                alarm,
            } => {
                violate(
                    "engineAppliesEveryCommit",
                    format!("record {at}: server {server} at epoch {epoch}: {alarm}"),
                );
            }
            Record::Acked {
                request,
                epoch,
                sequence,
            } => {
                acknowledged.push((at, request, *epoch, *sequence));
                if !log
                    .iter()
                    .any(|commit| commit.request_id.0 == request.as_bytes())
                {
                    violate(
                        "refinesChangeLog",
                        format!("record {at}: acknowledged {request} is not in the log"),
                    );
                }
            }
            Record::Exited {
                exit: Exit::Alarm(alarm),
                ..
            } => {
                let invariant = match alarm {
                    Alarm::OlderEpochAfterDataWrite { .. } => "dataWriteNeverSeesOlderEpoch",
                    Alarm::OtherContentInOwnEpoch { .. } => "oneContentPerSlotPerEpoch",
                    Alarm::UnreadableSegment { .. }
                    | Alarm::UnexpectedKey { .. }
                    | Alarm::PermanentStoreError { .. }
                    | Alarm::StartNotInLog { .. } => "noAlarm",
                };
                violate(invariant, format!("record {at}: {alarm:?}"));
            }
            Record::Panicked { server, epoch } => {
                violate(
                    "noPanic",
                    format!("record {at}: server {server} at epoch {epoch}"),
                );
            }
            Record::ProbeStarted => before_probe = Some(log.clone()),
            Record::ProbeFinished { acknowledged } => {
                if !acknowledged {
                    violate(
                        "takeoverCompletes",
                        format!(
                            "record {at}: the newest assignee did not acknowledge every request"
                        ),
                    );
                }
                if before_probe.as_ref() != Some(&log) {
                    violate(
                        "newestAssigneeSeesAcknowledged",
                        format!(
                            "record {at}: the newest assignee wrote an acknowledged request again"
                        ),
                    );
                }
            }
            Record::ProbeRecalculated { sequence } => {
                if let Some((_, request, _, newest)) = acknowledged
                    .iter()
                    .find(|(.., acked)| Some(*acked) > *sequence)
                {
                    violate(
                        "newestAssigneeRecalculatesAcknowledged",
                        format!(
                            "record {at}: the newest assignee recalculated up to {sequence:?}, not {request} at {newest:?}"
                        ),
                    );
                }
            }
            Record::Refused {
                request,
                reason: Refusal::Invalid,
            } => invalid.push((at, request)),
            Record::Granted { .. }
            | Record::Crashed { .. }
            | Record::Exited { .. }
            | Record::Rejected { .. }
            | Record::Refused { .. } => {}
        }
    }
    // Not checked against the log at the time of the record: a store over the network can show a write to a read-back before its PUT returns, and PassedOn is recorded by another task.
    let at = |sequence: Sequence| log.get(sequence.index());
    for (at_record, request, epoch, sequence) in acknowledged {
        if at(sequence).is_none_or(|commit| commit.request_id.0 != request.as_bytes()) {
            violate(
                "ackCarriesSequence",
                format!("record {at_record}: {request} acknowledged as {sequence:?}"),
            );
        }
        if epochs_that_pass_on.contains(&epoch)
            && passed_on.get(&epoch).map_or(0, Vec::len) <= sequence.index()
        {
            violate(
                "passedOnBeforeAck",
                format!(
                    "record {at_record}: epoch {epoch} acknowledged {request} as {sequence:?} without passing it on"
                ),
            );
        }
    }
    // Not for Busy: a later try of the same request can commit.
    for (at_record, request) in invalid {
        if log
            .iter()
            .any(|commit| commit.request_id.0 == request.as_bytes())
        {
            violate(
                "invalidNeverCommits",
                format!("record {at_record}: refused {request} as invalid, but it is in the log"),
            );
        }
    }
    for (epoch, commits) in passed_on {
        for ((at_record, commit), expected) in commits.into_iter().zip(0..) {
            if commit.sequence != Sequence(expected) || at(commit.sequence) != Some(commit) {
                violate(
                    "passedOnInLogOrder",
                    format!("record {at_record}: epoch {epoch} passed on {commit:?}"),
                );
            }
        }
    }
    for (epoch, sequences) in recalculated {
        let mut last = None;
        for (at_record, sequence) in sequences {
            if Some(sequence) <= last || at(sequence).is_none() {
                violate(
                    "recalculatedGrowsWithinLog",
                    format!(
                        "record {at_record}: epoch {epoch} recalculated {sequence:?} after {last:?} with {} commits in the log",
                        log.len()
                    ),
                );
            }
            last = Some(sequence);
        }
    }
    if store != outcome.store {
        violate(
            "historyMatchesStore",
            "the applied writes differ from the final store".to_owned(),
        );
    }
    violations
}

pub fn counts(outcome: &Outcome) -> (usize, usize) {
    (acknowledged(&outcome.history).len(), outcome.store.len())
}
