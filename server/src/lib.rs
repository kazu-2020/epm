mod edit;

use std::collections::BTreeSet;
use std::sync::Arc;

use change_log::{Commit, Sequence};
use datagen::Workspace;
use dimension::Dimension;
use evaluator::EvaluateError;
use formula::{AxisError, CheckedExpression, ResolveError, SyntaxError};
use measure::{Delta, Measure, MeasureDefinition, MeasureError, MeasureUlid};

pub use crate::edit::{DecodeError, Edit};

#[derive(Clone, Debug)]
pub struct Engine {
    dimensions: Arc<[Dimension]>,
    measures: Vec<Measure>,
    formulas: Vec<(MeasureUlid, CheckedExpression)>,
    inputs: BTreeSet<MeasureUlid>,
    next: Sequence,
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

#[derive(Debug, PartialEq)]
pub enum Rejection {
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
        let input_ulids = inputs.iter().map(Measure::ulid).collect();
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
            inputs: input_ulids,
            next: Sequence::default(),
        })
    }

    pub fn check(&self, edit: &Edit) -> Result<(), Rejection> {
        self.delta(edit).map(drop)
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
            edit.measure,
            delta,
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

    fn delta(&self, edit: &Edit) -> Result<Delta, Rejection> {
        if !self.inputs.contains(&edit.measure) {
            return Err(Rejection::NotAnInput(edit.measure));
        }
        let mut delta = self
            .measures
            .iter()
            .find(|measure| measure.ulid() == edit.measure)
            .expect("the engine holds every input measure")
            .delta();
        match edit.value {
            Some(value) => delta.set(&edit.coordinate, value),
            None => delta.delete(&edit.coordinate),
        }
        .map_err(Rejection::Coordinate)?;
        Ok(delta)
    }
}
