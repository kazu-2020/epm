mod apply;
mod assignee;
mod restore;

pub mod edit;
pub mod http;
pub mod snapshot;

use std::ops::RangeInclusive;

use change_log::{Commit, CommitCount, RequestId, Sequence, Slot, Start};
use dimension::Dimension;
use formula::EvaluateError;
use measure::{Measure, MeasureGlobalId};
use workspace::{CalculateError, Calculated, CellError, EditCheck, Edits, Workspace};

pub use crate::assignee::{
    Calculation, Handle, Lag, Loading, ReadError, Settings, Stopped, SubmitError, start,
};
pub use crate::edit::DecodeError;
pub use crate::restore::restore;
pub use workspace::Edit;

#[derive(Debug, PartialEq)]
pub enum Exit {
    ChangeLog(change_log::Exit),
    Engine(EngineAlarm),
}

#[derive(Clone, Debug)]
pub struct Engine {
    calculated: Calculated,
    applied: CommitCount,
    slot: Slot,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Invalid {
    Undecodable(DecodeError),
    NotAnInput(MeasureGlobalId),
    Cell(CellError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    Invalid(Invalid),
    Busy { behind: u64 },
}

#[derive(Debug, PartialEq)]
pub enum EngineAlarm {
    OutOfOrder {
        expected: Sequence,
        found: Sequence,
    },
    Invalid {
        sequence: Sequence,
        invalid: Invalid,
    },
    Panicked {
        sequences: RangeInclusive<Sequence>,
    },
    Failed {
        sequences: RangeInclusive<Sequence>,
        error: EvaluateError,
    },
}

impl Engine {
    pub fn new(workspace: Workspace) -> Result<Engine, CalculateError> {
        Engine::resume(workspace, &Start::default())
    }

    pub fn resume(workspace: Workspace, start: &Start) -> Result<Engine, CalculateError> {
        let calculated = Calculated::new(workspace)?;
        Ok(Engine {
            calculated,
            applied: CommitCount::of(start.request_ids.len()),
            slot: start.slot,
        })
    }

    pub fn dimensions(&self) -> &[Dimension] {
        self.calculated.dimensions()
    }

    pub fn axes<'e>(
        &'e self,
        measure: &'e Measure,
    ) -> impl ExactSizeIterator<Item = &'e Dimension> {
        measure.definition().axes().iter().map(|&axis| {
            self.dimensions()
                .iter()
                .find(|dimension| dimension.global_id() == axis)
                .expect("every measure of the engine is made from its dimensions")
        })
    }

    pub fn edit_check(&self) -> &EditCheck {
        self.calculated.edit_check()
    }

    pub fn apply_batch(&self, commits: &[(Slot, Commit)]) -> Result<Engine, EngineAlarm> {
        let (Some((_, first)), Some((slot, last))) = (commits.first(), commits.last()) else {
            return Ok(self.clone());
        };
        let mut applied = self.applied;
        let mut edits = Edits::default();
        for (_, commit) in commits {
            let sequence = commit.sequence;
            let expected = applied.next();
            if sequence != expected {
                return Err(EngineAlarm::OutOfOrder {
                    expected,
                    found: sequence,
                });
            }
            edit::add(self.edit_check(), &commit.payload, &mut edits)
                .map_err(|invalid| EngineAlarm::Invalid { sequence, invalid })?;
            applied = sequence.into();
        }
        let calculated = self
            .calculated
            .apply(edits)
            .map_err(|error| EngineAlarm::Failed {
                sequences: first.sequence..=last.sequence,
                error,
            })?;
        Ok(Engine {
            calculated,
            applied,
            slot: *slot,
        })
    }

    pub fn measures(&self) -> &[Measure] {
        self.calculated.measures()
    }

    pub fn recalculated(&self) -> Option<Sequence> {
        self.applied.last()
    }

    fn snapshot(&self, request_ids: &[RequestId]) -> snapshot::Snapshot {
        snapshot::Snapshot {
            start: Start {
                slot: self.slot,
                request_ids: if cfg!(feature = "snapshot_without_request_ids") {
                    Vec::new()
                } else if cfg!(feature = "snapshot_ahead") {
                    request_ids.to_vec()
                } else {
                    request_ids[..self.applied.length()].to_vec()
                },
            },
            inputs: self.calculated.inputs().to_vec(),
        }
    }
}
