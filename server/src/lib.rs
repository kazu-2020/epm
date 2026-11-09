mod edit;

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use change_log::{Admit, Commit, Epoch, Sequence, Submission};
use datagen::Workspace;
use dimension::Dimension;
use evaluator::EvaluateError;
use formula::{AxisError, CheckedExpression, ResolveError, SyntaxError};
use measure::{Delta, Measure, MeasureDefinition, MeasureError, MeasureUlid};
use object_store::ObjectStore;
use tokio::sync::{mpsc, watch};

pub use crate::edit::{DecodeError, Edit};

#[derive(Debug, PartialEq)]
pub enum Exit {
    ChangeLog(change_log::Exit),
    Engine(EngineAlarm),
}

// shortcut: every new owner rebuilds the engine from the start of the log; add snapshots when start-up time matters.
pub async fn run(
    store: Arc<dyn ObjectStore>,
    workspace: &str,
    epoch: Epoch,
    engine: Engine,
    requests: mpsc::Receiver<Submission<Rejection>>,
    recalculated: watch::Sender<Option<Sequence>>,
) -> Exit {
    let (accepted, accepted_requests) = mpsc::channel(1);
    let (committed, commits) = mpsc::unbounded_channel();
    let definitions = engine.definitions.clone();
    // Not two spawned tasks: a crash aborts this one future, so the engine stops with the owner.
    tokio::select! {
        biased;
        exit = change_log::serve(store, workspace, epoch, definitions, accepted_requests, committed) => {
            Exit::ChangeLog(exit)
        }
        alarm = recalculate(engine, requests, accepted, commits, recalculated) => {
            Exit::Engine(alarm)
        }
    }
}

async fn recalculate(
    mut engine: Engine,
    mut requests: mpsc::Receiver<Submission<Rejection>>,
    accepted: mpsc::Sender<Submission<Rejection>>,
    mut commits: mpsc::UnboundedReceiver<Commit>,
    recalculated: watch::Sender<Option<Sequence>>,
) -> EngineAlarm {
    let mut accepted = Some(accepted);
    loop {
        tokio::select! {
            biased;
            Some(commit) = commits.recv() => {
                // Not after the edit: yielding first lets the tasks that wait for this commit's ACK run before the engine works on it.
                tokio::task::yield_now().await;
                if cfg!(feature = "skip_commit") && commit.sequence == Sequence(1) {
                    continue;
                }
                if cfg!(feature = "swap_commits")
                    && let Ok(later) = commits.try_recv()
                    && let Err(alarm) = engine.apply(&later)
                {
                    return alarm;
                }
                if let Err(alarm) = engine.apply(&commit) {
                    return alarm;
                }
                if cfg!(feature = "apply_twice") && let Err(alarm) = engine.apply(&commit) {
                    return alarm;
                }
                recalculated.send_replace(Some(commit.sequence));
            }
            submission = requests.recv(), if accepted.is_some() => match submission {
                Some(submission) => {
                    if cfg!(feature = "apply_before_commit") {
                        let early = Commit {
                            request_id: submission.request.request_id.clone(),
                            sequence: engine.next,
                            payload: submission.request.payload.clone(),
                        };
                        if let Err(alarm) = engine.apply(&early) {
                            return alarm;
                        }
                        recalculated.send_replace(Some(early.sequence));
                    }
                    if let Some(accepted) = &accepted {
                        // Not an error: the owner stopped, and run returns its exit.
                        let _ = accepted.send(submission).await;
                    }
                }
                None => accepted = None,
            },
            else => std::future::pending::<()>().await,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Engine {
    dimensions: Arc<[Dimension]>,
    measures: Vec<Measure>,
    formulas: Vec<(MeasureUlid, CheckedExpression)>,
    definitions: Definitions,
    next: Sequence,
}

#[derive(Clone, Debug)]
pub struct Definitions {
    inputs: BTreeMap<MeasureUlid, Delta>,
}

#[derive(Debug, PartialEq)]
pub enum FormulaError {
    Syntax {
        result: MeasureUlid,
        errors: Vec<SyntaxError>,
    },
    Resolve {
        result: MeasureUlid,
        errors: Vec<ResolveError>,
    },
    Axes {
        result: MeasureUlid,
        errors: Vec<AxisError>,
    },
    Evaluate {
        result: MeasureUlid,
        error: EvaluateError,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    Undecodable(DecodeError),
    NotAnInput(MeasureUlid),
    Coordinate(MeasureError),
}

#[derive(Debug, PartialEq)]
pub enum EngineAlarm {
    OutOfOrder {
        expected: Sequence,
        found: Sequence,
    },
    Undecodable {
        sequence: Sequence,
        error: DecodeError,
    },
    Rejected {
        sequence: Sequence,
        rejection: Rejection,
    },
    Failed {
        sequence: Sequence,
        error: EvaluateError,
    },
}

impl Engine {
    pub fn new(workspace: Workspace) -> Result<Engine, FormulaError> {
        let Workspace {
            dimensions,
            inputs,
            formulas,
        } = workspace;
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let definitions: Vec<MeasureDefinition> = inputs
            .iter()
            .map(|input| input.definition().clone())
            .chain(formulas.iter().map(|formula| formula.result.clone()))
            .collect();
        let input_definitions = Definitions {
            inputs: inputs
                .iter()
                .map(|input| (input.ulid(), input.delta()))
                .collect(),
        };
        let mut measures = inputs;
        let mut checked = Vec::with_capacity(formulas.len());
        for formula in &formulas {
            let result = formula.result.ulid;
            let parsed = formula::parse(&formula.text);
            if !parsed.errors.is_empty() {
                let errors = parsed.errors;
                return Err(FormulaError::Syntax { result, errors });
            }
            let resolved = formula::resolve(&parsed.expression, &definitions, &references);
            if !resolved.errors.is_empty() {
                let errors = resolved.errors;
                return Err(FormulaError::Resolve { result, errors });
            }
            let axes = formula::check_axes(&resolved.expression, &formula.result.axes);
            let Some(expression) = axes.checked else {
                let errors = axes.errors;
                return Err(FormulaError::Axes { result, errors });
            };
            let sources: Vec<&Measure> = measures.iter().collect();
            let evaluated =
                column_engine::evaluate(&expression, &formula.result, &sources, &references)
                    .map_err(|error| FormulaError::Evaluate { result, error })?;
            measures.push(evaluated);
            checked.push((result, expression));
        }
        Ok(Engine {
            dimensions: dimensions.into(),
            measures,
            formulas: checked,
            definitions: input_definitions,
            next: Sequence::default(),
        })
    }

    pub fn definitions(&self) -> &Definitions {
        &self.definitions
    }

    pub fn apply(&mut self, commit: &Commit) -> Result<(), EngineAlarm> {
        let sequence = commit.sequence;
        if sequence != self.next {
            return Err(EngineAlarm::OutOfOrder {
                expected: self.next,
                found: sequence,
            });
        }
        let edit = Edit::decode(commit.payload.clone())
            .map_err(|error| EngineAlarm::Undecodable { sequence, error })?;
        let delta = self
            .definitions
            .delta(&edit)
            .map_err(|rejection| EngineAlarm::Rejected {
                sequence,
                rejection,
            })?;
        let dimensions: Vec<&Dimension> = self.dimensions.iter().collect();
        let formulas: Vec<(MeasureUlid, &CheckedExpression)> = self
            .formulas
            .iter()
            .map(|(result, expression)| (*result, expression))
            .collect();
        column_engine::edit(
            &mut self.measures,
            [(edit.measure, delta)],
            &formulas,
            &dimensions,
        )
        .map_err(|error| EngineAlarm::Failed { sequence, error })?;
        self.next = sequence.next();
        Ok(())
    }

    pub fn measures(&self) -> &[Measure] {
        &self.measures
    }
}

impl Definitions {
    fn delta(&self, edit: &Edit) -> Result<Delta, Rejection> {
        let mut delta = self
            .inputs
            .get(&edit.measure)
            .ok_or(Rejection::NotAnInput(edit.measure))?
            .clone();
        match edit.value {
            Some(value) => delta.set(&edit.coordinate, value),
            None => delta.delete(&edit.coordinate),
        }
        .map_err(Rejection::Coordinate)?;
        Ok(delta)
    }
}

impl Admit for Definitions {
    type Rejection = Rejection;

    fn admit(&mut self, payload: &Bytes) -> Result<(), Rejection> {
        let edit = Edit::decode(payload.clone()).map_err(Rejection::Undecodable)?;
        self.delta(&edit).map(drop)
    }
}
