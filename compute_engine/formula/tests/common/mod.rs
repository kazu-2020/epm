use std::ops::Range;

use dimension::{AttributeDefinition, AttributeUlid, DimensionDefinition, DimensionUlid, KeyKind};
use tile::{TileDefinition, TileUlid};
use ulid::Ulid;

const PERIOD: DimensionUlid = DimensionUlid(Ulid(1));
const DEPARTMENT: DimensionUlid = DimensionUlid(Ulid(2));
const DIVISION: DimensionUlid = DimensionUlid(Ulid(3));
const VERSION: DimensionUlid = DimensionUlid(Ulid(4));

pub fn dimensions() -> Vec<DimensionDefinition> {
    let dimension = |ulid, name: &str, attributes| DimensionDefinition {
        ulid,
        name: name.into(),
        key_kind: KeyKind::Name,
        attributes,
    };
    vec![
        dimension(PERIOD, "期間", vec![]),
        dimension(
            DEPARTMENT,
            "部門",
            vec![AttributeDefinition {
                ulid: AttributeUlid(Ulid(21)),
                name: "事業部".into(),
                target: DIVISION,
            }],
        ),
        dimension(DIVISION, "事業部", vec![]),
        dimension(VERSION, "バージョン", vec![]),
    ]
}

pub fn tiles() -> Vec<TileDefinition> {
    let tile = |ulid, name: &str, axes: &[DimensionUlid]| TileDefinition {
        ulid: TileUlid(Ulid(ulid)),
        name: name.into(),
        axes: axes.to_vec(),
    };
    vec![
        tile(11, "売上", &[DEPARTMENT, PERIOD]),
        tile(12, "売上計画", &[DEPARTMENT, PERIOD]),
        tile(13, "事業部売上", &[DIVISION, PERIOD]),
        tile(14, "全社売上", &[PERIOD]),
        tile(15, "配賦率", &[DIVISION]),
        tile(16, "内部取引", &[DEPARTMENT, DIVISION, PERIOD]),
        tile(17, "uriage", &[PERIOD]),
    ]
}

pub fn dimension_name(ulid: DimensionUlid) -> String {
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
