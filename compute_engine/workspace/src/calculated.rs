use std::collections::BTreeMap;
use std::sync::Arc;

use dimension::Dimension;
use formula::{CheckedExpression, EvaluateError};
use measure::{DeltaBuilder, Measure, MeasureGlobalId};

use crate::cell::{CellError, Coordinate, resolve};
use crate::compile::CompiledFormula;
use crate::workspace::Workspace;

#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub measure: MeasureGlobalId,
    // Not the member indexes in the declared order: the change log keeps an edit, and a later definition can declare the axes in another order or move a member to another row (#184).
    pub coordinate: Coordinate,
    pub value: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EditError {
    NotAnInput(MeasureGlobalId),
    Cell(CellError),
}

#[derive(Debug, PartialEq)]
pub struct CalculateError {
    pub result: MeasureGlobalId,
    pub error: EvaluateError,
}

#[derive(Clone, Debug)]
pub struct EditCheck {
    inputs: Arc<BTreeMap<MeasureGlobalId, Input>>,
    dimensions: Arc<[Dimension]>,
}

#[derive(Debug)]
struct Input {
    empty: DeltaBuilder,
    axis_positions: Vec<usize>,
}

#[derive(Debug, Default)]
pub struct Edits {
    deltas: BTreeMap<MeasureGlobalId, DeltaBuilder>,
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
        self.inputs.contains_key(&measure)
    }

    pub fn check(&self, edit: &Edit) -> Result<(), EditError> {
        self.add(edit, &mut Edits::default())
    }

    pub fn add(&self, edit: &Edit, edits: &mut Edits) -> Result<(), EditError> {
        let input = self
            .inputs
            .get(&edit.measure)
            .ok_or(EditError::NotAnInput(edit.measure))?;
        let axes = input
            .axis_positions
            .iter()
            .map(|&position| &self.dimensions[position]);
        let coordinate = resolve(axes, &edit.coordinate).map_err(EditError::Cell)?;
        let delta = edits
            .deltas
            .entry(edit.measure)
            .or_insert_with(|| input.empty.clone());
        match edit.value {
            Some(value) => delta.set(&coordinate, value),
            None => delta.delete(&coordinate),
        }
        .expect(
            "Workspace::try_new checks that the member counts of every input match its dimensions",
        );
        Ok(())
    }
}

impl Calculated {
    pub fn new(workspace: Workspace) -> Result<Calculated, CalculateError> {
        let (dimensions, inputs, formulas) = workspace.into_compiled();
        let dimensions: Arc<[Dimension]> = dimensions.into();
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let edit_check = EditCheck {
            inputs: Arc::new(
                inputs
                    .iter()
                    .map(|input| {
                        let axis_positions = input
                            .definition()
                            .axes()
                            .iter()
                            .map(|&axis| {
                                dimensions
                                    .iter()
                                    .position(|dimension| dimension.global_id() == axis)
                                    .expect("Workspace::try_new checks that every input is made from its dimensions")
                            })
                            .collect();
                        let empty = input.delta();
                        (input.global_id(), Input { empty, axis_positions })
                    })
                    .collect(),
            ),
            dimensions: Arc::clone(&dimensions),
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
                result: formula.result().global_id(),
                error,
            })?;
            measures.push(result);
        }
        Ok(Calculated {
            dimensions,
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
            .map(|formula| (formula.result().global_id(), formula.expression()))
            .collect();
        let measures = column_engine::edit(
            &self.measures,
            edits
                .deltas
                .into_iter()
                .map(|(measure, delta)| (measure, delta.finish())),
            &formulas,
            &dimensions,
        )?
        .iter()
        .map(Measure::fold_deltas_or_compact)
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
