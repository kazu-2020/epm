use dimension::Dimension;
use measure::{Measure, MeasureDefinition};

pub struct Workspace {
    pub dimensions: Vec<Dimension>,
    pub inputs: Vec<Measure>,
    pub formulas: Vec<Formula>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    pub result: MeasureDefinition,
    pub text: String,
}
