use std::sync::Arc;

use arrow_array::{ArrayRef, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
use dimension::{
    AttributeDefinition, AttributeUlid, Dimension, DimensionDefinition, DimensionUlid, KeyKind,
    MemberId,
};
use formula::{CheckedExpression, ResolvedExpression};
use measure::{Measure, MeasureDefinition, MeasureUlid};
use ulid::Ulid;

const PERIOD: DimensionUlid = DimensionUlid(Ulid(1));
const DEPARTMENT: DimensionUlid = DimensionUlid(Ulid(2));
const DIVISION: DimensionUlid = DimensionUlid(Ulid(3));

type Cells = [(&'static [u32], f64)];

pub struct Case {
    pub name: &'static str,
    pub steps: &'static [(&'static str, &'static str)],
    pub expected: &'static Cells,
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
        name: "先頭の項の符号反転は、引き算と同じ値になる",
        steps: &[("結果", "-'売上' + '原価'")],
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
        name: "数値はメジャーにあるセルにだけ足し、セルを増やさない",
        steps: &[("結果", "'売上' + 1")],
        expected: &[
            (&[0, 0], 101.0),
            (&[0, 1], 201.0),
            (&[1, 0], 51.0),
            (&[2, 1], 8.0),
        ],
    },
    Case {
        name: "軸の少ないメジャーは全軸のメジャーにあるセルにだけ足し、無ければ0として扱う",
        steps: &[("結果", "'売上' + '人数'")],
        expected: &[
            (&[0, 0], 102.0),
            (&[0, 1], 202.0),
            (&[1, 0], 53.0),
            (&[2, 1], 7.0),
        ],
    },
    Case {
        name: "軸の少ない側が左でもセルは全軸のメジャーから",
        steps: &[("結果", "'人数' - '売上'")],
        expected: &[
            (&[0, 0], -98.0),
            (&[0, 1], -198.0),
            (&[1, 0], -47.0),
            (&[2, 1], -7.0),
        ],
    },
    Case {
        name: "どの項も全軸を持たない足し算は、各項を足りない軸の全メンバーに広げて足す",
        steps: &[("結果", "'人数' + SUM('売上', BY '期間')")],
        expected: &[
            (&[0, 0], 152.0),
            (&[0, 1], 209.0),
            (&[1, 0], 153.0),
            (&[1, 1], 210.0),
            (&[2, 0], 150.0),
            (&[2, 1], 207.0),
        ],
    },
    Case {
        name: "足し算と引き算だけの式は括弧や符号反転の付け方によらず、全軸を持つ項のセルだけを作る",
        steps: &[("結果", "'売上' + -('人数' + SUM('原価', BY '期間'))")],
        expected: &[
            (&[0, 0], 73.0),
            (&[0, 1], 178.0),
            (&[1, 0], 22.0),
            (&[2, 1], -13.0),
        ],
    },
    Case {
        name: "掛け算は右にしかない軸も両辺のあるセルだけに広げる",
        steps: &[("結果", "'人数' * '売上'")],
        expected: &[(&[0, 0], 200.0), (&[0, 1], 400.0), (&[1, 0], 150.0)],
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
        name: "SUM は座標の小さい順に足す",
        steps: &[("期間結果", "SUM('桁落ち')")],
        expected: &[(&[0], 1.0), (&[1], 1.0)],
    },
    Case {
        name: "空のメジャーの SUM は空",
        steps: &[("期間結果", "SUM('空')")],
        expected: &[],
    },
    Case {
        name: "入力は未反映の差分を反映して読む",
        steps: &[("結果", "'改定'")],
        expected: &[(&[0, 0], 5.0), (&[2, 1], 3.0)],
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

impl Default for Fixture {
    fn default() -> Self {
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
        let input_cells: [(MeasureDefinition, &Cells); 6] = [
            (
                measure_definition(11, "売上", &[DEPARTMENT, PERIOD]),
                &[
                    (&[0, 0], 100.0),
                    (&[0, 1], 200.0),
                    (&[1, 0], 50.0),
                    (&[2, 1], 7.0),
                ],
            ),
            (
                measure_definition(12, "原価", &[DEPARTMENT, PERIOD]),
                &[(&[0, 0], 25.0), (&[1, 0], 0.0), (&[1, 1], 20.0)],
            ),
            (
                measure_definition(13, "人数", &[DEPARTMENT]),
                &[(&[0], 2.0), (&[1], 3.0)],
            ),
            (measure_definition(14, "空", &[DEPARTMENT, PERIOD]), &[]),
            (
                measure_definition(15, "改定", &[DEPARTMENT, PERIOD]),
                &[(&[0, 0], 1.0), (&[1, 0], 2.0)],
            ),
            (
                measure_definition(16, "桁落ち", &[DEPARTMENT]),
                // 1 added next to ±1e16 is rounded away, so only coordinate order, which cancels the two large values first, keeps it.
                &[(&[0], 1e16), (&[1], -1e16), (&[2], 1.0)],
            ),
        ];
        let results = [
            measure_definition(31, "結果", &[DEPARTMENT, PERIOD]),
            measure_definition(32, "部門結果", &[DEPARTMENT]),
            measure_definition(33, "期間結果", &[PERIOD]),
            measure_definition(34, "事業部結果", &[DIVISION, PERIOD]),
            measure_definition(35, "中間", &[DEPARTMENT, PERIOD]),
        ];
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let mut inputs: Vec<Measure> = input_cells
            .iter()
            .map(|(definition, cells)| measure(definition.clone(), cells, &references))
            .collect();
        let measure_definitions = input_cells
            .into_iter()
            .map(|(definition, _)| definition)
            .chain(results)
            .collect();
        let revised = inputs
            .iter_mut()
            .find(|input| input.definition().name == "改定")
            .expect("the fixture defines '改定'");
        let mut delta = revised.delta();
        delta.set(&[MemberId(0), MemberId(0)], 5.0).unwrap();
        delta.delete(&[MemberId(1), MemberId(0)]).unwrap();
        delta.set(&[MemberId(2), MemberId(1)], 3.0).unwrap();
        *revised = revised.apply(delta).unwrap();
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
}

impl Fixture {
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

    pub fn check(&self, source: &str, result: &str) -> CheckedExpression {
        let check = formula::check_axes(&self.resolve(source), &self.definition(result).axes);
        assert_eq!(check.errors, [], "{source}");
        check.checked.expect("a formula without errors is checked")
    }
}

pub fn exact(cells: &[(Vec<MemberId>, f64)]) -> Vec<(Vec<u32>, String)> {
    // Not the f64 itself: 0.0 == -0.0, and the cases must tell them apart.
    cells
        .iter()
        .map(|(coordinate, value)| {
            let members = coordinate.iter().map(|member| member.0).collect();
            (members, format!("{value:?}"))
        })
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
    let cells = cells
        .iter()
        .map(|&(coordinate, value)| (coordinate.iter().copied().map(MemberId).collect(), value))
        .collect();
    Measure::from_cells(definition, cells, dimensions).unwrap()
}
