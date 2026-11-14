use std::collections::BTreeSet;

use dimension::{Dimension, DimensionGlobalId};
use formula::{AxisError, CheckedAxis, CheckedExpression, CheckedNode, ResolveError, SyntaxError};
use measure::{CoordinatePacking, MeasureDefinition, MeasureGlobalId};

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
    DuplicateAxis(DimensionGlobalId),
    MissingDimension(DimensionGlobalId),
    PackingTooWide(Vec<DimensionGlobalId>),
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
        let mut sources = BTreeSet::new();
        collect_sources(self.expression.root(), &mut sources);
        sources
    }
}

pub fn compile(
    text: &str,
    result: &MeasureDefinition,
    measures: &[MeasureDefinition],
    dimensions: &[&Dimension],
) -> Result<CheckedExpression, FormulaError> {
    let mut declared = BTreeSet::new();
    if let Some(&duplicate) = result.axes.iter().find(|&&axis| !declared.insert(axis)) {
        return Err(FormulaError::DuplicateAxis(duplicate));
    }
    let parsed = formula::parse(text);
    if !parsed.errors.is_empty() {
        return Err(FormulaError::Syntax(parsed.errors));
    }
    let resolved = formula::resolve(&parsed.expression, measures, dimensions);
    if !resolved.errors.is_empty() {
        return Err(FormulaError::Resolve(resolved.errors));
    }
    let check = formula::check_axes(&resolved.expression, &result.axes);
    let Some(expression) = check.checked else {
        return Err(FormulaError::Axes(check.errors));
    };
    fits(result.axes.iter().copied().collect(), dimensions)?;
    node_axes(expression.root(), dimensions)?;
    Ok(expression)
}

fn collect_sources(node: &CheckedNode, sources: &mut BTreeSet<MeasureGlobalId>) {
    match node {
        CheckedNode::Number(_) => {}
        CheckedNode::Measure { global_id, .. } => {
            sources.insert(*global_id);
        }
        CheckedNode::Terms(terms) => {
            for term in terms {
                collect_sources(&term.node, sources);
            }
        }
        CheckedNode::Product { left, right, .. } => {
            collect_sources(left, sources);
            collect_sources(right, sources);
        }
        CheckedNode::Sum { value, .. } | CheckedNode::Filter { value, .. } => {
            collect_sources(value, sources);
        }
    }
}

// Not left to the evaluation: a result in the middle of a formula that is wider than 64 bits (ADR 0004) would stop the assignee at the first edit.
fn node_axes(
    node: &CheckedNode,
    dimensions: &[&Dimension],
) -> Result<BTreeSet<DimensionGlobalId>, FormulaError> {
    let axes = match node {
        CheckedNode::Number(_) => BTreeSet::new(),
        CheckedNode::Measure { axes, .. } => axes.iter().copied().collect(),
        CheckedNode::Terms(terms) => {
            let mut axes = BTreeSet::new();
            for term in terms {
                axes.extend(node_axes(&term.node, dimensions)?);
            }
            axes
        }
        CheckedNode::Product { left, right, .. } => {
            let mut axes = node_axes(left, dimensions)?;
            axes.extend(node_axes(right, dimensions)?);
            axes
        }
        CheckedNode::Sum { value, by } => {
            node_axes(value, dimensions)?;
            by.iter()
                .map(|axis| match *axis {
                    CheckedAxis::Dimension(dimension) => dimension,
                    CheckedAxis::Attribute { target, .. } => target,
                })
                .collect()
        }
        CheckedNode::Filter {
            value,
            dimension,
            keeps_axis,
            ..
        } => {
            let mut axes = node_axes(value, dimensions)?;
            if !keeps_axis {
                axes.remove(dimension);
            }
            axes
        }
    };
    fits(axes, dimensions)
}

fn fits(
    axes: BTreeSet<DimensionGlobalId>,
    dimensions: &[&Dimension],
) -> Result<BTreeSet<DimensionGlobalId>, FormulaError> {
    let member_counts = axes
        .iter()
        .map(|&axis| {
            dimensions
                .iter()
                .find(|dimension| dimension.global_id() == axis)
                .map(|dimension| dimension.len())
                .ok_or(FormulaError::MissingDimension(axis))
        })
        .collect::<Result<Vec<u32>, FormulaError>>()?;
    if CoordinatePacking::new(&member_counts).is_none() {
        return Err(FormulaError::PackingTooWide(axes.into_iter().collect()));
    }
    Ok(axes)
}
