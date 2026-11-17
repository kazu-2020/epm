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

pub(crate) fn dimensions() -> Vec<DimensionDefinition> {
    let dimension =
        |global_id: DimensionGlobalId, name: &str, attributes: Vec<AttributeDefinition>| {
            let natural_key_attribute = AttributeDefinition {
                global_id: AttributeGlobalId(Ulid(global_id.0.0 * 1000)),
                name: "ID".into(),
                kind: AttributeKind::Text,
            };
            DimensionDefinition::try_new(
                global_id,
                name.into(),
                DimensionKind::Ordinary,
                natural_key_attribute.global_id,
                [vec![natural_key_attribute], attributes].concat(),
            )
            .expect("the fixture defines a valid dimension")
        };
    vec![
        dimension(PERIOD, "期間", vec![]),
        dimension(DIVISION, "事業部", vec![]),
        dimension(
            DEPARTMENT,
            "部門",
            vec![AttributeDefinition {
                global_id: AttributeGlobalId(Ulid(21)),
                name: "事業部".into(),
                kind: AttributeKind::Reference(DIVISION),
            }],
        ),
        dimension(VERSION, "バージョン", vec![]),
    ]
}

pub(crate) fn dimensions_with_members() -> Vec<Dimension> {
    let mut loaded: Vec<Dimension> = Vec::new();
    for definition in dimensions() {
        let (natural_keys, divisions): (&[&str], &[u32]) = match definition.name() {
            "期間" => (&["1月", "2月"], &[]),
            "部門" => (&["営業", "開発"], &[0, 0]),
            "事業部" => (&["第一"], &[]),
            _ => (&["実績", "予算", "見込"], &[]),
        };
        let global_ids = FixedSizeBinaryArray::try_from_iter((0..natural_keys.len()).map(|row| {
            Ulid(
                definition.global_id().0.0 * 100
                    + u128::try_from(row).expect("a row index fits in u128"),
            )
            .to_bytes()
        }))
        .expect("every member global id has the same width");
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(global_ids),
            Arc::new(StringArray::from(natural_keys.to_vec())),
        ];
        if definition.attributes().len() > 1 {
            columns.push(Arc::new(UInt32Array::from(divisions.to_vec())));
        }
        let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns)
            .expect("the columns match the dimension schema");
        let targets: Vec<&Dimension> = loaded.iter().collect();
        let dimension = Dimension::try_new(definition, batch, &targets)
            .expect("the fixture defines valid members");
        loaded.push(dimension);
    }
    loaded
}

pub(crate) fn measures() -> Vec<MeasureDefinition> {
    let measure = |global_id, name: &str, axes: &[DimensionGlobalId]| {
        MeasureDefinition::try_new(MeasureGlobalId(Ulid(global_id)), name.into(), axes.to_vec())
            .expect("the fixture defines a valid measure")
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

pub(crate) fn dimension_name(global_id: DimensionGlobalId) -> String {
    dimensions()
        .into_iter()
        .find(|dimension| dimension.global_id() == global_id)
        .expect("the fixture defines every dimension it refers to")
        .name()
        .into()
}

pub(crate) fn span(source: &str, part: &str) -> Range<usize> {
    let start = source.rfind(part).expect("the part is in the source");
    start..start + part.len()
}
