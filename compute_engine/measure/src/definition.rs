use std::collections::HashSet;
use std::fmt;

use arrow_schema::{DataType, Field, Schema};
use dimension::DimensionGlobalId;
use ulid::Ulid;

const VALUE_COLUMN: &str = "value";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeasureGlobalId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureDefinition {
    global_id: MeasureGlobalId,
    name: String,
    axes: Vec<DimensionGlobalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionError {
    NoAxes,
    DuplicateAxis(DimensionGlobalId),
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for DefinitionError {}

impl MeasureDefinition {
    pub fn try_new(
        global_id: MeasureGlobalId,
        name: String,
        axes: Vec<DimensionGlobalId>,
    ) -> Result<Self, DefinitionError> {
        if axes.is_empty() {
            return Err(DefinitionError::NoAxes);
        }
        let mut seen = HashSet::new();
        if let Some(&duplicate) = axes.iter().find(|&&axis| !seen.insert(axis)) {
            return Err(DefinitionError::DuplicateAxis(duplicate));
        }
        Ok(Self {
            global_id,
            name,
            axes,
        })
    }

    pub fn global_id(&self) -> MeasureGlobalId {
        self.global_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn axes(&self) -> &[DimensionGlobalId] {
        &self.axes
    }

    pub fn schema(&self) -> Schema {
        let mut fields: Vec<Field> = self
            .axes
            .iter()
            .map(|axis| Field::new(axis.0.to_string(), DataType::UInt32, false))
            .collect();
        fields.push(Field::new(VALUE_COLUMN, DataType::Float64, false));
        Schema::new(fields)
    }
}
