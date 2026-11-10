mod apply;
mod edit;
mod owner;

pub mod http;

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use bytes::Bytes;
use change_log::{Commit, Sequence};
use dimension::Dimension;
use evaluator::EvaluateError;
use formula::{AxisError, CheckedExpression, ResolveError, SyntaxError};
use measure::{Delta, Measure, MeasureDefinition, MeasureError, MeasureUlid};
use workspace::Workspace;

pub use crate::edit::{DecodeError, Edit};
pub use crate::owner::{Calculation, Handle, Lag, ReadError, Settings, SubmitError, start};

#[derive(Debug, PartialEq)]
pub enum Exit {
    ChangeLog(change_log::Exit),
    Engine(EngineAlarm),
}

#[derive(Clone, Debug)]
pub struct Engine {
    dimensions: Arc<[Dimension]>,
    measures: Vec<Measure>,
    formulas: Arc<[(MeasureUlid, CheckedExpression)]>,
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
pub enum Invalid {
    Undecodable(DecodeError),
    NotAnInput(MeasureUlid),
    Coordinate(MeasureError),
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
            formulas: checked.into(),
            definitions: input_definitions,
            next: Sequence::default(),
        })
    }

    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    pub fn definitions(&self) -> &Definitions {
        &self.definitions
    }

    pub fn apply_batch(&self, commits: &[Commit]) -> Result<Engine, EngineAlarm> {
        let (Some(first), Some(last)) = (commits.first(), commits.last()) else {
            return Ok(self.clone());
        };
        let mut next = self.next;
        let mut deltas = BTreeMap::new();
        for commit in commits {
            let sequence = commit.sequence;
            if sequence != next {
                return Err(EngineAlarm::OutOfOrder {
                    expected: next,
                    found: sequence,
                });
            }
            self.definitions
                .add(&commit.payload, &mut deltas)
                .map_err(|invalid| EngineAlarm::Invalid { sequence, invalid })?;
            next = sequence.next();
        }
        let dimensions: Vec<&Dimension> = self.dimensions.iter().collect();
        let formulas: Vec<(MeasureUlid, &CheckedExpression)> = self
            .formulas
            .iter()
            .map(|(result, expression)| (*result, expression))
            .collect();
        let measures = column_engine::edit(&self.measures, deltas, &formulas, &dimensions)
            .map_err(|error| EngineAlarm::Failed {
                sequences: first.sequence..=last.sequence,
                error,
            })?
            .iter()
            .map(|measure| {
                if measure.delta_len() > 0 {
                    measure.fold_deltas()
                } else {
                    measure.clone()
                }
            })
            .collect();
        Ok(Engine {
            dimensions: Arc::clone(&self.dimensions),
            measures,
            formulas: Arc::clone(&self.formulas),
            definitions: self.definitions.clone(),
            next,
        })
    }

    pub fn measures(&self) -> &[Measure] {
        &self.measures
    }

    pub fn recalculated(&self) -> Option<Sequence> {
        self.next.0.checked_sub(1).map(Sequence)
    }
}

impl Definitions {
    pub fn is_input(&self, measure: MeasureUlid) -> bool {
        self.inputs.contains_key(&measure)
    }

    pub fn check(&self, payload: &Bytes) -> Result<(), Invalid> {
        self.add(payload, &mut BTreeMap::new())
    }

    fn add(
        &self,
        payload: &Bytes,
        deltas: &mut BTreeMap<MeasureUlid, Delta>,
    ) -> Result<(), Invalid> {
        let edit = Edit::decode(payload.clone()).map_err(Invalid::Undecodable)?;
        let empty = self
            .inputs
            .get(&edit.measure)
            .ok_or(Invalid::NotAnInput(edit.measure))?;
        let delta = deltas.entry(edit.measure).or_insert_with(|| empty.clone());
        match edit.value {
            Some(value) => delta.set(&edit.coordinate, value),
            None => delta.delete(&edit.coordinate),
        }
        .map_err(Invalid::Coordinate)
    }
}
