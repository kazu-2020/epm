use std::collections::BTreeSet;

use dimension::Dimension;
use formula::{AxisError, CheckedExpression, EvaluateError, ResolveError, SyntaxError};
use measure::{MeasureDefinition, MeasureGlobalId};

#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    pub result: MeasureDefinition,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledFormula {
    formula: Formula,
    expression: CheckedExpression,
}

#[derive(Debug, PartialEq)]
pub enum FormulaError {
    Syntax(Vec<SyntaxError>),
    Resolve(Vec<ResolveError>),
    Axes(Vec<AxisError>),
    Plan(EvaluateError),
}

impl CompiledFormula {
    pub(crate) fn new(
        formula: Formula,
        measures: &[MeasureDefinition],
        dimensions: &[&Dimension],
    ) -> Result<CompiledFormula, FormulaError> {
        let expression = compile(&formula.text, &formula.result, measures, dimensions)?;
        Ok(CompiledFormula {
            formula,
            expression,
        })
    }

    pub fn result(&self) -> &MeasureDefinition {
        &self.formula.result
    }

    pub fn text(&self) -> &str {
        &self.formula.text
    }

    pub fn expression(&self) -> &CheckedExpression {
        &self.expression
    }

    pub(crate) fn sources(&self) -> BTreeSet<MeasureGlobalId> {
        self.expression.sources()
    }
}

pub fn compile(
    text: &str,
    result: &MeasureDefinition,
    measures: &[MeasureDefinition],
    dimensions: &[&Dimension],
) -> Result<CheckedExpression, FormulaError> {
    let parsed = formula::parse(text);
    if !parsed.errors.is_empty() {
        return Err(FormulaError::Syntax(parsed.errors));
    }
    let resolved = formula::resolve(&parsed.expression, measures, dimensions);
    if !resolved.errors.is_empty() {
        return Err(FormulaError::Resolve(resolved.errors));
    }
    let check = formula::check_axes(&resolved.expression, result.axes());
    let Some(expression) = check.checked else {
        return Err(FormulaError::Axes(check.errors));
    };
    // Not left to the first edit: a result in the middle of a formula that is wider than 64 bits (ADR 0004) would stop the assignee there.
    column_engine::plan(&expression, result, dimensions).map_err(FormulaError::Plan)?;
    Ok(expression)
}
