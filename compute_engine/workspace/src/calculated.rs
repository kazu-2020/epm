use std::collections::BTreeMap;
use std::sync::Arc;

use dimension::{Dimension, MemberIndex};
use formula::{CheckedExpression, EvaluateError};
use measure::{CoordinateError, Delta, Measure, MeasureGlobalId};

use crate::compile::CompiledFormula;
use crate::workspace::Workspace;

#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub measure: MeasureGlobalId,
    pub coordinate: Vec<MemberIndex>,
    pub value: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EditError {
    NotAnInput(MeasureGlobalId),
    Coordinate(CoordinateError),
}

#[derive(Debug, PartialEq)]
pub struct CalculateError {
    pub result: MeasureGlobalId,
    pub error: EvaluateError,
}

#[derive(Clone, Debug)]
pub struct EditCheck {
    empty: Arc<BTreeMap<MeasureGlobalId, Delta>>,
}

#[derive(Debug, Default)]
pub struct Edits {
    deltas: BTreeMap<MeasureGlobalId, Delta>,
}

#[derive(Clone, Debug)]
pub struct Calculated {
    dimensions: Arc<[Dimension]>,
    formulas: Arc<[CompiledFormula]>,
    measures: Vec<Measure>,
    input_count: usize,
    edit_check: EditCheck,
}

impl EditCheck {
    pub fn is_input(&self, measure: MeasureGlobalId) -> bool {
        self.empty.contains_key(&measure)
    }

    pub fn check(&self, edit: &Edit) -> Result<(), EditError> {
        self.add(edit, &mut Edits::default())
    }

    pub fn add(&self, edit: &Edit, edits: &mut Edits) -> Result<(), EditError> {
        let empty = self
            .empty
            .get(&edit.measure)
            .ok_or(EditError::NotAnInput(edit.measure))?;
        let delta = edits
            .deltas
            .entry(edit.measure)
            .or_insert_with(|| empty.clone());
        match edit.value {
            Some(value) => delta.set(&edit.coordinate, value),
            None => delta.delete(&edit.coordinate),
        }
        .map_err(EditError::Coordinate)
    }
}

impl Calculated {
    pub fn new(workspace: Workspace) -> Result<Calculated, CalculateError> {
        let (dimensions, inputs, formulas) = workspace.into_compiled();
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let edit_check = EditCheck {
            empty: Arc::new(
                inputs
                    .iter()
                    .map(|input| (input.ulid(), input.delta()))
                    .collect(),
            ),
        };
        let input_count = inputs.len();
        let mut measures = inputs;
        for formula in &formulas {
            let sources: Vec<&Measure> = measures.iter().collect();
            let result = column_engine::evaluate(
                formula.expression(),
                formula.result(),
                &sources,
                &references,
            )
            .map_err(|error| CalculateError {
                result: formula.result().ulid,
                error,
            })?;
            measures.push(result);
        }
        Ok(Calculated {
            dimensions: dimensions.into(),
            formulas: formulas.into(),
            measures,
            input_count,
            edit_check,
        })
    }

    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    pub fn inputs(&self) -> &[Measure] {
        &self.measures[..self.input_count]
    }

    pub fn measures(&self) -> &[Measure] {
        &self.measures
    }

    pub fn edit_check(&self) -> &EditCheck {
        &self.edit_check
    }

    pub fn apply(&self, edits: Edits) -> Result<Calculated, EvaluateError> {
        let dimensions: Vec<&Dimension> = self.dimensions.iter().collect();
        let formulas: Vec<(MeasureGlobalId, &CheckedExpression)> = self
            .formulas
            .iter()
            .map(|formula| (formula.result().ulid, formula.expression()))
            .collect();
        let measures = column_engine::edit(&self.measures, edits.deltas, &formulas, &dimensions)?
            .iter()
            .map(Measure::fold_deltas)
            .collect();
        Ok(Calculated {
            dimensions: Arc::clone(&self.dimensions),
            formulas: Arc::clone(&self.formulas),
            measures,
            input_count: self.input_count,
            edit_check: self.edit_check.clone(),
        })
    }
}
