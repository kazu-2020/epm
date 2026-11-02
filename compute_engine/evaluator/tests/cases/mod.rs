use std::sync::Arc;

use arrow_array::{
    ArrayRef, FixedSizeBinaryArray, Float64Array, RecordBatch, StringArray, UInt32Array,
};
use dimension::{
    AttributeDefinition, AttributeUlid, Dimension, DimensionDefinition, DimensionUlid, KeyKind,
};
use formula::ResolvedExpression;
use measure::{Measure, MeasureDefinition, MeasureUlid};
use ulid::Ulid;

const PERIOD: DimensionUlid = DimensionUlid(Ulid(1));
const DEPARTMENT: DimensionUlid = DimensionUlid(Ulid(2));
const DIVISION: DimensionUlid = DimensionUlid(Ulid(3));

pub struct Case {
    pub name: &'static str,
    pub steps: &'static [(&'static str, &'static str)],
    pub expected: &'static [(&'static [u32], f64)],
}

pub const CASES: &[Case] = &[
    Case {
        name: "足し算は空のセルを0として扱い、両方空なら空",
        steps: &[("結果", "'売上' + '原価'")],
        expected: &[
            (&[0, 0], 125.0),
            (&[0, 1], 200.0),
            (&[1, 0], 50.0),
            (&[1, 1], 20.0),
            (&[2, 1], 7.0),
        ],
    },
    Case {
        name: "空どうしの足し算は空",
        steps: &[("結果", "'空' + '空'")],
        expected: &[],
    },
    Case {
        name: "引き算は空のセルを0として扱う",
        steps: &[("結果", "'原価' - '売上'")],
        expected: &[
            (&[0, 0], -75.0),
            (&[0, 1], -200.0),
            (&[1, 0], -50.0),
            (&[1, 1], 20.0),
            (&[2, 1], -7.0),
        ],
    },
    Case {
        name: "掛け算はどちらかが空なら空",
        steps: &[("結果", "'売上' * '原価'")],
        expected: &[(&[0, 0], 2500.0), (&[1, 0], 0.0)],
    },
    Case {
        name: "0で割ると空",
        steps: &[("結果", "'売上' / '原価'")],
        expected: &[(&[0, 0], 4.0)],
    },
    Case {
        name: "0を0で割ると空",
        steps: &[("結果", "'原価' / '原価'")],
        expected: &[(&[0, 0], 1.0), (&[1, 1], 1.0)],
    },
    Case {
        name: "数値の0で割るとすべて空",
        steps: &[("結果", "'売上' / 0")],
        expected: &[],
    },
    Case {
        name: "符号反転は空のセルを空のままにする",
        steps: &[("結果", "-'売上'")],
        expected: &[
            (&[0, 0], -100.0),
            (&[0, 1], -200.0),
            (&[1, 0], -50.0),
            (&[2, 1], -7.0),
        ],
    },
    Case {
        name: "符号反転でできた-0は0になる",
        steps: &[("結果", "-'原価'")],
        expected: &[(&[0, 0], -25.0), (&[1, 0], 0.0), (&[1, 1], -20.0)],
    },
    Case {
        name: "数値のブロードキャストで疎なメジャーが密になる",
        steps: &[("結果", "'売上' + 1")],
        expected: &[
            (&[0, 0], 101.0),
            (&[0, 1], 201.0),
            (&[1, 0], 51.0),
            (&[1, 1], 1.0),
            (&[2, 0], 1.0),
            (&[2, 1], 8.0),
        ],
    },
    Case {
        name: "軸の少ないメジャーは足りない軸の全メンバーに広がる",
        steps: &[("結果", "'売上' + '人数'")],
        expected: &[
            (&[0, 0], 102.0),
            (&[0, 1], 202.0),
            (&[1, 0], 53.0),
            (&[1, 1], 3.0),
            (&[2, 1], 7.0),
        ],
    },
    Case {
        name: "SUM BY はディメンションごとに合計し、セルのない組は空",
        steps: &[("部門結果", "SUM('原価', BY '部門')")],
        expected: &[(&[0], 25.0), (&[1], 20.0)],
    },
    Case {
        name: "SUM BY は残す軸を選ぶ",
        steps: &[("期間結果", "SUM('売上', BY '期間')")],
        expected: &[(&[0], 150.0), (&[1], 207.0)],
    },
    Case {
        name: "属性で付け替え、属性が空のメンバーは合計から外す",
        steps: &[("事業部結果", "SUM('売上', BY '部門'.'事業部', '期間')")],
        expected: &[(&[0, 0], 150.0), (&[0, 1], 200.0)],
    },
    Case {
        name: "BY のない SUM は結果の軸にブロードキャストする",
        steps: &[("期間結果", "SUM('売上')")],
        expected: &[(&[0], 357.0), (&[1], 357.0)],
    },
    Case {
        name: "空のメジャーの SUM は空",
        steps: &[("期間結果", "SUM('空')")],
        expected: &[],
    },
    Case {
        name: "計算メジャーを次の数式が参照する",
        steps: &[
            ("中間", "'売上' * 2"),
            ("期間結果", "SUM('中間', BY '期間')"),
        ],
        expected: &[(&[0], 300.0), (&[1], 414.0)],
    },
];

pub struct Fixture {
    pub dimensions: Vec<Dimension>,
    pub inputs: Vec<Measure>,
    dimension_definitions: Vec<DimensionDefinition>,
    measure_definitions: Vec<MeasureDefinition>,
}

impl Fixture {
    pub fn new() -> Self {
        let periods = dimension(PERIOD, "期間", &["4月", "5月"], vec![], &[], &[]);
        let divisions = dimension(
            DIVISION,
            "事業部",
            &["A事業部", "B事業部"],
            vec![],
            &[],
            &[],
        );
        let departments = dimension(
            DEPARTMENT,
            "部門",
            &["営業部", "開発部", "管理部"],
            vec![AttributeDefinition {
                ulid: AttributeUlid(Ulid(21)),
                name: "事業部".into(),
                target: DIVISION,
            }],
            &[Some(0), Some(0), None],
            &[&divisions],
        );
        let dimensions = vec![periods, departments, divisions];
        let measure_definitions = vec![
            measure_definition(11, "売上", &[DEPARTMENT, PERIOD]),
            measure_definition(12, "原価", &[DEPARTMENT, PERIOD]),
            measure_definition(13, "人数", &[DEPARTMENT]),
            measure_definition(14, "空", &[DEPARTMENT, PERIOD]),
            measure_definition(31, "結果", &[DEPARTMENT, PERIOD]),
            measure_definition(32, "部門結果", &[DEPARTMENT]),
            measure_definition(33, "期間結果", &[PERIOD]),
            measure_definition(34, "事業部結果", &[DIVISION, PERIOD]),
            measure_definition(35, "中間", &[DEPARTMENT, PERIOD]),
        ];
        let cells: [&[(&[u32], f64)]; 4] = [
            &[
                (&[0, 0], 100.0),
                (&[0, 1], 200.0),
                (&[1, 0], 50.0),
                (&[2, 1], 7.0),
            ],
            &[(&[0, 0], 25.0), (&[1, 0], 0.0), (&[1, 1], 20.0)],
            &[(&[0], 2.0), (&[1], 3.0)],
            &[],
        ];
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let inputs = measure_definitions
            .iter()
            .zip(cells)
            .map(|(definition, cells)| measure(definition.clone(), cells, &references))
            .collect();
        Fixture {
            dimension_definitions: dimensions
                .iter()
                .map(|dimension| dimension.definition().clone())
                .collect(),
            dimensions,
            inputs,
            measure_definitions,
        }
    }

    pub fn definition(&self, name: &str) -> &MeasureDefinition {
        self.measure_definitions
            .iter()
            .find(|definition| definition.name == name)
            .expect("the fixture defines every measure a case names")
    }

    pub fn resolve(&self, source: &str) -> ResolvedExpression {
        let parsed = formula::parse(source);
        assert_eq!(parsed.errors, [], "{source}");
        let resolved = formula::resolve(
            &parsed.expression,
            &self.measure_definitions,
            &self.dimension_definitions,
        );
        assert_eq!(resolved.errors, [], "{source}");
        resolved.expression
    }
}

pub fn exact(rows: &[(Vec<u32>, f64)]) -> Vec<(Vec<u32>, String)> {
    // Not the f64 itself: 0.0 == -0.0, and the cases must tell them apart.
    rows.iter()
        .map(|(coordinate, value)| (coordinate.clone(), format!("{value:?}")))
        .collect()
}

pub fn expected(case: &Case) -> Vec<(Vec<u32>, String)> {
    case.expected
        .iter()
        .map(|&(coordinate, value)| (coordinate.to_vec(), format!("{value:?}")))
        .collect()
}

fn dimension(
    ulid: DimensionUlid,
    name: &str,
    names: &[&str],
    attributes: Vec<AttributeDefinition>,
    attribute_values: &[Option<u32>],
    targets: &[&Dimension],
) -> Dimension {
    let definition = DimensionDefinition {
        ulid,
        name: name.into(),
        key_kind: KeyKind::Name,
        attributes,
    };
    let ulids = FixedSizeBinaryArray::try_from_iter(
        (0..names.len()).map(|row| Ulid(ulid.0.0 * 100 + row as u128).to_bytes()),
    )
    .unwrap();
    let mut columns: Vec<ArrayRef> =
        vec![Arc::new(ulids), Arc::new(StringArray::from(names.to_vec()))];
    if !definition.attributes.is_empty() {
        columns.push(Arc::new(UInt32Array::from(attribute_values.to_vec())));
    }
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    Dimension::try_new(definition, batch, targets).unwrap()
}

fn measure_definition(ulid: u128, name: &str, axes: &[DimensionUlid]) -> MeasureDefinition {
    MeasureDefinition {
        ulid: MeasureUlid(Ulid(ulid)),
        name: name.into(),
        axes: axes.to_vec(),
    }
}

fn measure(
    definition: MeasureDefinition,
    cells: &[(&[u32], f64)],
    dimensions: &[&Dimension],
) -> Measure {
    let mut columns: Vec<ArrayRef> = (0..definition.axes.len())
        .map(|axis| {
            Arc::new(UInt32Array::from_iter_values(
                cells.iter().map(|(coordinate, _)| coordinate[axis]),
            )) as ArrayRef
        })
        .collect();
    columns.push(Arc::new(Float64Array::from_iter_values(
        cells.iter().map(|&(_, value)| value),
    )));
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    Measure::try_new(definition, batch, dimensions).unwrap()
}
