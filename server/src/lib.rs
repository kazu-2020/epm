mod apply;
mod assignee;
mod restore;

pub mod edit;
pub mod http;
pub mod snapshot;

use std::ops::RangeInclusive;

use bytes::Bytes;
use change_log::{Commit, RequestId, Sequence, Slot, Start};
use dimension::Dimension;
use formula::EvaluateError;
use measure::{CoordinateError, Measure, MeasureGlobalId};
use workspace::{CalculateError, Calculated, EditCheck, EditError, Edits, Workspace};

pub use crate::assignee::{Calculation, Handle, Lag, ReadError, Settings, SubmitError, start};
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
    definitions: Definitions,
    next: Sequence,
    slot: Slot,
}

#[derive(Clone, Debug)]
pub struct Definitions {
    inputs: EditCheck,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Invalid {
    Undecodable(DecodeError),
    NotAnInput(MeasureGlobalId),
    Coordinate(CoordinateError),
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
            definitions: Definitions {
                inputs: calculated.edit_check().clone(),
            },
            calculated,
            next: Sequence(u64::try_from(start.request_ids.len()).expect("a count fits in u64")),
            slot: start.slot,
        })
    }

    pub fn dimensions(&self) -> &[Dimension] {
        self.calculated.dimensions()
    }

    pub fn definitions(&self) -> &Definitions {
        &self.definitions
    }

    pub fn apply_batch(&self, commits: &[(Slot, Commit)]) -> Result<Engine, EngineAlarm> {
        let (Some((_, first)), Some((slot, last))) = (commits.first(), commits.last()) else {
            return Ok(self.clone());
        };
        let mut next = self.next;
        let mut edits = Edits::default();
        for (_, commit) in commits {
            let sequence = commit.sequence;
            if sequence != next {
                return Err(EngineAlarm::OutOfOrder {
                    expected: next,
                    found: sequence,
                });
            }
            self.definitions
                .add(&commit.payload, &mut edits)
                .map_err(|invalid| EngineAlarm::Invalid { sequence, invalid })?;
            next = sequence.next();
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
            definitions: self.definitions.clone(),
            next,
            slot: *slot,
        })
    }

    pub fn measures(&self) -> &[Measure] {
        self.calculated.measures()
    }

    pub fn recalculated(&self) -> Option<Sequence> {
        self.next.0.checked_sub(1).map(Sequence)
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
                    request_ids[..self.next.index()].to_vec()
                },
            },
            inputs: self.calculated.inputs().to_vec(),
        }
    }
}

impl Definitions {
    pub fn is_input(&self, measure: MeasureGlobalId) -> bool {
        self.inputs.is_input(measure)
    }

    pub fn check(&self, payload: &Bytes) -> Result<(), Invalid> {
        self.add(payload, &mut Edits::default())
    }

    fn add(&self, payload: &Bytes, edits: &mut Edits) -> Result<(), Invalid> {
        let edit = edit::decode(payload.clone()).map_err(Invalid::Undecodable)?;
        self.inputs.add(&edit, edits).map_err(|error| match error {
            EditError::NotAnInput(measure) => Invalid::NotAnInput(measure),
            EditError::Coordinate(error) => Invalid::Coordinate(error),
        })
    }
}
