use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use change_log::{Epoch, Exit, Sequence, Slot};

use super::check::{acknowledged, decode, replay, slot_of};
use super::simulate::{Outcome, Record};

pub fn witnesses(outcome: &Outcome) -> BTreeSet<&'static str> {
    let mut seen = BTreeSet::new();
    let mut sent: BTreeMap<&str, Vec<&Bytes>> = BTreeMap::new();
    let mut epochs: BTreeMap<usize, u64> = BTreeMap::new();
    let mut applied: BTreeSet<(u64, Sequence)> = BTreeSet::new();
    let history = &outcome.history;
    let (segments, _) = replay(&outcome.store);
    let granted = |flag: fn(bool, bool) -> bool| -> BTreeSet<u64> {
        history
            .iter()
            .filter_map(|record| match record {
                Record::Granted {
                    epoch,
                    passes_on,
                    recalculates,
                    ..
                } if flag(*passes_on, *recalculates) => Some(*epoch),
                _ => None,
            })
            .collect()
    };
    let passes_on = granted(|passes_on, _| passes_on);
    let recalculates = granted(|_, recalculates| recalculates);
    let replied: BTreeSet<(u64, Sequence)> = history
        .iter()
        .filter_map(|record| match record {
            Record::PassedOn { epoch, commit } => Some((*epoch, commit.sequence)),
            Record::Acked {
                epoch, sequence, ..
            } if !passes_on.contains(epoch) => Some((*epoch, *sequence)),
            _ => None,
        })
        .collect();
    let newest = |pick: fn(&Record) -> Option<(u64, Sequence)>| {
        let mut newest: BTreeMap<u64, Sequence> = BTreeMap::new();
        for (epoch, sequence) in history.iter().filter_map(pick) {
            let entry = newest.entry(epoch).or_insert(sequence);
            *entry = (*entry).max(sequence);
        }
        newest
    };
    let acked = newest(|record| match record {
        Record::Acked {
            epoch, sequence, ..
        } => Some((*epoch, *sequence)),
        _ => None,
    });
    let recalculated = newest(|record| match record {
        Record::Recalculated { epoch, sequence } => Some((*epoch, *sequence)),
        _ => None,
    });
    for record in history {
        match record {
            Record::Sent { key, body } => sent.entry(key).or_default().push(body),
            Record::Granted { server, epoch, .. } => {
                epochs.insert(*server, *epoch);
            }
            Record::Applied { body, .. } => {
                let segment = decode(body);
                if let Some(first) = segment.commits.first() {
                    applied.insert((segment.epoch.0, first.sequence));
                }
            }
            Record::Crashed { server } => {
                let Some(&epoch) = epochs.get(server) else {
                    continue;
                };
                if applied
                    .iter()
                    .any(|written| written.0 == epoch && !replied.contains(written))
                {
                    seen.insert("crashAfterApplyBeforeReply");
                }
                if recalculates.contains(&epoch) && acked.get(&epoch) > recalculated.get(&epoch) {
                    seen.insert("crashBeforeRecalculated");
                }
            }
            Record::Exited {
                exit: Exit::Stopped,
                ..
            } => {
                seen.insert("oldOwnerStopped");
            }
            Record::Rejected { .. } => {
                seen.insert("storeRejectedPut");
            }
            _ => {}
        }
    }
    if acknowledged(&outcome.history).len() >= 2 {
        seen.insert("twoAcknowledged");
    }
    if segments
        .values()
        .any(|segment| segment.epoch.0 >= 2 && !segment.commits.is_empty())
    {
        seen.insert("commitAfterHandover");
    }
    if sent.values().any(|bodies| {
        bodies
            .iter()
            .enumerate()
            .any(|(i, body)| bodies[..i].contains(body))
    }) {
        seen.insert("sameBytesResent");
    }
    let fences: BTreeSet<(Epoch, Slot)> = sent
        .iter()
        .flat_map(|(key, bodies)| bodies.iter().map(|body| (slot_of(key), decode(body))))
        .filter(|(_, segment)| segment.commits.is_empty())
        .map(|(slot, segment)| (segment.epoch, slot))
        .collect();
    if fences
        .iter()
        .any(|(epoch, slot)| fences.contains(&(*epoch, Slot(slot.0 + 1))))
    {
        seen.insert("fenceMovedPastOldOwner");
    }
    if fences.iter().any(|(epoch, slot)| {
        fences.contains(&(*epoch, Slot(slot.0 + 1)))
            && segments
                .get(slot)
                .is_some_and(|older| older.epoch < *epoch && !older.commits.is_empty())
    }) {
        seen.insert("fenceFoundOlderCommits");
    }
    seen
}
