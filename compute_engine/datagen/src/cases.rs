use std::sync::Arc;

use crate::natural_key_attribute;
use arrow_array::{ArrayRef, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, Dimension, DimensionDefinition,
    DimensionGlobalId, DimensionKind, MemberIndex,
};
use formula::{CheckedExpression, ResolvedExpression};
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use ulid::Ulid;

const PERIOD: DimensionGlobalId = DimensionGlobalId(Ulid(1));
const DEPARTMENT: DimensionGlobalId = DimensionGlobalId(Ulid(2));
const DIVISION: DimensionGlobalId = DimensionGlobalId(Ulid(3));

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
        name: "左の項の軸が少なくても、セルは全軸を持つ右の項から作る",
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
        name: "数値は結果のすべての軸の全メンバーに広げる",
        steps: &[("結果", "1")],
        expected: &[
            (&[0, 0], 1.0),
            (&[0, 1], 1.0),
            (&[1, 0], 1.0),
            (&[1, 1], 1.0),
            (&[2, 0], 1.0),
            (&[2, 1], 1.0),
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
        name: "属性でリマップし、属性が空のメンバーは合計から外す",
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
        name: "1メンバーの絞り込みは、そのディメンションの軸を消す",
        steps: &[("部門結果", "'売上'['期間': '4月']")],
        expected: &[(&[0], 100.0), (&[1], 50.0)],
    },
    Case {
        name: "1メンバーの絞り込みどうしは、消えた軸の違うメンバーを引き算できる",
        steps: &[("部門結果", "'売上'['期間': '5月'] - '売上'['期間': '4月']")],
        expected: &[(&[0], 100.0), (&[1], -50.0), (&[2], 7.0)],
    },
    Case {
        name: "複数メンバーの絞り込みは軸を残し、選んだメンバーのセルだけを残す",
        steps: &[(
            "期間結果",
            "SUM(FILTER('売上', '部門' IN ('営業部', '管理部')), BY '期間')",
        )],
        expected: &[(&[0], 100.0), (&[1], 207.0)],
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
                global_id: AttributeGlobalId(Ulid(21)),
                name: "事業部".into(),
                kind: AttributeKind::Reference(DIVISION),
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
                measure_definition(16, "桁落ち", &[DEPARTMENT, PERIOD]),
                // Not declared in packing order: adding the cells by 部門 first, as declared, rounds 1 away next to 1e16, while 期間 first cancels the two large values before 1.
                &[(&[0, 0], 1e16), (&[0, 1], 1.0), (&[1, 0], -1e16)],
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
        delta.set(&[MemberIndex(0), MemberIndex(0)], 5.0).unwrap();
        delta.delete(&[MemberIndex(1), MemberIndex(0)]).unwrap();
        delta.set(&[MemberIndex(2), MemberIndex(1)], 3.0).unwrap();
        *revised = revised.apply(delta).unwrap();
        Fixture {
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
            &self.dimensions.iter().collect::<Vec<_>>(),
        );
        assert_eq!(resolved.errors, [], "{source}");
        resolved.expression
    }

    pub fn check(&self, source: &str, result: &str) -> CheckedExpression {
        let compiled = workspace::compile(
            source,
            self.definition(result),
            &self.measure_definitions,
            &self.dimensions.iter().collect::<Vec<_>>(),
        );
        compiled.unwrap_or_else(|error| panic!("{source}: {error:?}"))
    }
}

pub fn exact(cells: &[(Vec<MemberIndex>, f64)]) -> Vec<(Vec<u32>, String)> {
    // Not the f64 itself: 0.0 == -0.0, and the cases must tell them apart.
    let mut cells: Vec<(Vec<u32>, String)> = cells
        .iter()
        .map(|(coordinate, value)| {
            let members = coordinate.iter().map(|member| member.0).collect();
            (members, format!("{value:?}"))
        })
        .collect();
    // Not the given order: a measure lists its cells in packing order, and the cases and the oracle list them by the declared axes.
    cells.sort();
    cells
}

pub fn expected(case: &Case) -> Vec<(Vec<u32>, String)> {
    case.expected
        .iter()
        .map(|&(coordinate, value)| (coordinate.to_vec(), format!("{value:?}")))
        .collect()
}

fn dimension(
    global_id: DimensionGlobalId,
    name: &str,
    natural_keys: &[&str],
    attributes: Vec<AttributeDefinition>,
    attribute_values: &[Option<u32>],
    targets: &[&Dimension],
) -> Dimension {
    let definition = DimensionDefinition::try_new(
        global_id,
        name.into(),
        if global_id == PERIOD {
            DimensionKind::Calendar
        } else {
            DimensionKind::Ordinary
        },
        natural_key_attribute(global_id).global_id,
        [vec![natural_key_attribute(global_id)], attributes].concat(),
    )
    .unwrap();
    let global_ids = FixedSizeBinaryArray::try_from_iter(
        (0..natural_keys.len()).map(|row| Ulid(global_id.0.0 * 100 + row as u128).to_bytes()),
    )
    .unwrap();
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(global_ids),
        Arc::new(StringArray::from(natural_keys.to_vec())),
    ];
    if definition.attributes().len() > 1 {
        columns.push(Arc::new(UInt32Array::from(attribute_values.to_vec())));
    }
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    Dimension::try_new(definition, batch, targets).unwrap()
}

fn measure_definition(
    global_id: u128,
    name: &str,
    axes: &[DimensionGlobalId],
) -> MeasureDefinition {
    MeasureDefinition {
        global_id: MeasureGlobalId(Ulid(global_id)),
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
        .map(|&(coordinate, value)| (coordinate.iter().copied().map(MemberIndex).collect(), value))
        .collect();
    Measure::from_cells(definition, cells, dimensions).unwrap()
}
