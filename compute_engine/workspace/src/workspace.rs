use std::collections::{BTreeMap, BTreeSet, HashSet};

use dimension::{Dimension, DimensionGlobalId};
use measure::{Measure, MeasureDefinition, MeasureGlobalId};

use crate::compile::{CompiledFormula, Formula, FormulaError};
use crate::dependency_order::dependency_order;

pub struct UncheckedWorkspace {
    pub dimensions: Vec<Dimension>,
    pub inputs: Vec<Measure>,
    pub formulas: Vec<Formula>,
}

#[derive(Debug)]
pub struct Workspace {
    dimensions: Vec<Dimension>,
    inputs: Vec<Measure>,
    formulas: Vec<CompiledFormula>,
}

#[derive(Debug, PartialEq)]
pub enum WorkspaceError {
    DuplicateDimension(DimensionGlobalId),
    DuplicateMeasure(MeasureGlobalId),
    DuplicateName(String),
    InputOfOtherDimensions(MeasureGlobalId),
    Formula {
        result: MeasureGlobalId,
        error: FormulaError,
    },
    Cycle(Vec<MeasureGlobalId>),
}

impl Workspace {
    pub fn try_new(unchecked: UncheckedWorkspace) -> Result<Workspace, WorkspaceError> {
        let UncheckedWorkspace {
            dimensions,
            inputs,
            formulas,
        } = unchecked;
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let definitions: Vec<MeasureDefinition> = inputs
            .iter()
            .map(|input| input.definition().clone())
            .chain(formulas.iter().map(|formula| formula.result.clone()))
            .collect();
        let mut dimension_ids = HashSet::new();
        if let Some(duplicate) = dimensions
            .iter()
            .find(|dimension| !dimension_ids.insert(dimension.ulid()))
        {
            return Err(WorkspaceError::DuplicateDimension(duplicate.ulid()));
        }
        let mut measure_ids = HashSet::new();
        if let Some(duplicate) = definitions
            .iter()
            .find(|definition| !measure_ids.insert(definition.ulid))
        {
            return Err(WorkspaceError::DuplicateMeasure(duplicate.ulid));
        }
        // Not one set of names for each kind: a formula names a dimension and a measure in the same way, so the same name for both would pick one by its place in the formula.
        let mut names = HashSet::new();
        let dimension_names = dimensions.iter().map(Dimension::name);
        let measure_names = definitions
            .iter()
            .map(|definition| definition.name.as_str());
        if let Some(duplicate) = dimension_names
            .chain(measure_names)
            .find(|name| !names.insert(*name))
        {
            return Err(WorkspaceError::DuplicateName(duplicate.to_owned()));
        }
        if let Some(input) = inputs.iter().find(|input| !made_from(input, &references)) {
            return Err(WorkspaceError::InputOfOtherDimensions(input.ulid()));
        }
        let mut compiled = BTreeMap::new();
        for formula in formulas {
            let result = formula.result.ulid;
            let formula = CompiledFormula::new(formula, &definitions, &references)
                .map_err(|error| WorkspaceError::Formula { result, error })?;
            compiled.insert(result, formula);
        }
        let sources: BTreeMap<MeasureGlobalId, BTreeSet<MeasureGlobalId>> = compiled
            .iter()
            .map(|(&result, formula)| (result, formula.sources()))
            .collect();
        let formulas = dependency_order(&sources)
            .map_err(WorkspaceError::Cycle)?
            .into_iter()
            .filter_map(|result| compiled.remove(&result))
            .collect();
        Ok(Workspace {
            dimensions,
            inputs,
            formulas,
        })
    }

    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    pub fn inputs(&self) -> &[Measure] {
        &self.inputs
    }

    pub fn formulas(&self) -> &[CompiledFormula] {
        &self.formulas
    }

    pub fn with_inputs(self, mut inputs: Vec<Measure>) -> Result<Workspace, Workspace> {
        // Not the given order: a snapshot keeps the inputs in the order of the workspace that wrote it.
        inputs.sort_by_key(|input| {
            self.inputs
                .iter()
                .position(|declared| declared.ulid() == input.ulid())
        });
        let references: Vec<&Dimension> = self.dimensions.iter().collect();
        let same_definitions = inputs
            .iter()
            .map(Measure::definition)
            .eq(self.inputs.iter().map(Measure::definition));
        if !same_definitions || !inputs.iter().all(|input| made_from(input, &references)) {
            return Err(self);
        }
        Ok(Workspace { inputs, ..self })
    }

    pub(crate) fn into_compiled(self) -> (Vec<Dimension>, Vec<Measure>, Vec<CompiledFormula>) {
        (self.dimensions, self.inputs, self.formulas)
    }
}

fn made_from(input: &Measure, dimensions: &[&Dimension]) -> bool {
    let axes: Option<Vec<&Dimension>> = input
        .axis_member_counts()
        .map(|(axis, member_count)| {
            dimensions
                .iter()
                .copied()
                .find(|dimension| dimension.ulid() == axis && dimension.len() == member_count)
        })
        .collect();
    // Not the member counts alone: a dimension of another kind packs at another place, so the packed coordinates would read as other cells (ADR 0005).
    axes.is_some_and(|axes| axes.is_sorted_by_key(|dimension| dimension.packing_order()))
}
