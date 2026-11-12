use std::ops::Range;
use std::sync::Arc;

use arrow_array::{ArrayRef, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, Dimension, DimensionDefinition,
    DimensionGlobalId, DimensionKind,
};
use measure::{MeasureDefinition, MeasureGlobalId};
use ulid::Ulid;

const PERIOD: DimensionGlobalId = DimensionGlobalId(Ulid(1));
const DEPARTMENT: DimensionGlobalId = DimensionGlobalId(Ulid(2));
const DIVISION: DimensionGlobalId = DimensionGlobalId(Ulid(3));
const VERSION: DimensionGlobalId = DimensionGlobalId(Ulid(4));

pub fn dimensions() -> Vec<DimensionDefinition> {
    let dimension = |ulid: DimensionGlobalId, name: &str, attributes: Vec<AttributeDefinition>| {
        let natural_key_attribute = AttributeDefinition {
            ulid: AttributeGlobalId(Ulid(ulid.0.0 * 1000)),
            name: "ID".into(),
            kind: AttributeKind::Text,
        };
        DimensionDefinition {
            ulid,
            name: name.into(),
            kind: DimensionKind::Ordinary,
            natural_key_attribute: natural_key_attribute.ulid,
            attributes: [vec![natural_key_attribute], attributes].concat(),
        }
    };
    vec![
        dimension(PERIOD, "期間", vec![]),
        dimension(DIVISION, "事業部", vec![]),
        dimension(
            DEPARTMENT,
            "部門",
            vec![AttributeDefinition {
                ulid: AttributeGlobalId(Ulid(21)),
                name: "事業部".into(),
                kind: AttributeKind::Reference(DIVISION),
            }],
        ),
        dimension(VERSION, "バージョン", vec![]),
    ]
}

pub fn dimensions_with_members() -> Vec<Dimension> {
    let mut loaded: Vec<Dimension> = Vec::new();
    for definition in dimensions() {
        let (names, divisions): (&[&str], &[u32]) = match definition.name.as_str() {
            "期間" => (&["1月", "2月"], &[]),
            "部門" => (&["営業", "開発"], &[0, 0]),
            "事業部" => (&["第一"], &[]),
            _ => (&["実績", "予算", "見込"], &[]),
        };
        let ulids = FixedSizeBinaryArray::try_from_iter(
            (0..names.len()).map(|row| Ulid(definition.ulid.0.0 * 100 + row as u128).to_bytes()),
        )
        .unwrap();
        let mut columns: Vec<ArrayRef> =
            vec![Arc::new(ulids), Arc::new(StringArray::from(names.to_vec()))];
        if definition.attributes.len() > 1 {
            columns.push(Arc::new(UInt32Array::from(divisions.to_vec())));
        }
        let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
        let targets: Vec<&Dimension> = loaded.iter().collect();
        let dimension = Dimension::try_new(definition, batch, &targets).unwrap();
        loaded.push(dimension);
    }
    loaded
}

pub fn measures() -> Vec<MeasureDefinition> {
    let measure = |ulid, name: &str, axes: &[DimensionGlobalId]| MeasureDefinition {
        ulid: MeasureGlobalId(Ulid(ulid)),
        name: name.into(),
        axes: axes.to_vec(),
    };
    vec![
        measure(11, "売上", &[DEPARTMENT, PERIOD]),
        measure(12, "売上計画", &[DEPARTMENT, PERIOD]),
        measure(13, "事業部売上", &[DIVISION, PERIOD]),
        measure(14, "全社売上", &[PERIOD]),
        measure(15, "配賦率", &[DIVISION]),
        measure(16, "内部取引", &[DEPARTMENT, DIVISION, PERIOD]),
        measure(17, "uriage", &[PERIOD]),
    ]
}

pub fn dimension_name(ulid: DimensionGlobalId) -> String {
    dimensions()
        .into_iter()
        .find(|dimension| dimension.ulid == ulid)
        .expect("the fixture defines every dimension it refers to")
        .name
}

pub fn span(source: &str, part: &str) -> Range<usize> {
    let start = source.rfind(part).expect("the part is in the source");
    start..start + part.len()
}
