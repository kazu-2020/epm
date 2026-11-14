use arrow_schema::{DataType, Field, Schema};
use dimension::DimensionGlobalId;
use ulid::Ulid;

const VALUE_COLUMN: &str = "value";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeasureGlobalId(pub Ulid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureDefinition {
    pub global_id: MeasureGlobalId,
    pub name: String,
    pub axes: Vec<DimensionGlobalId>,
}

impl MeasureDefinition {
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
