use std::collections::{BTreeSet, HashMap};

use column_engine::Region;
use datagen::Workspace;
use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::{Dimension, DimensionUlid, MemberId};
use formula::CheckedExpression;
use measure::{Measure, MeasureDefinition, MeasureUlid};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use ulid::Ulid;

const SMALL: Size = Size {
    products: 30,
    customers: 20,
    departments_per_kind: 4,
    quantity_cells: 3_000,
    account_fill: 0.5,
};

const CATEGORY: DimensionUlid = DimensionUlid(Ulid(1));
const GROUP: DimensionUlid = DimensionUlid(Ulid(2));
const ACCOUNT: DimensionUlid = DimensionUlid(Ulid(3));
const DEPARTMENT: DimensionUlid = DimensionUlid(Ulid(5));
const PRODUCT: DimensionUlid = DimensionUlid(Ulid(7));
const CUSTOMER: DimensionUlid = DimensionUlid(Ulid(8));
const PERIOD: DimensionUlid = DimensionUlid(Ulid(9));
const VERSION: DimensionUlid = DimensionUlid(Ulid(10));

struct Model {
    workspace: Workspace,
    checked: Vec<(MeasureDefinition, CheckedExpression)>,
}

impl Model {
    fn new() -> Model {
        Model::with_results_reversed(false)
    }

    fn with_results_reversed(reversed: bool) -> Model {
        let mut workspace = representative::generate(&SMALL, 7);
        if reversed {
            for formula in &mut workspace.formulas {
                formula.result.axes.reverse();
            }
        }
        let dimensions: Vec<&Dimension> = workspace.dimensions.iter().collect();
        let definitions: Vec<MeasureDefinition> = workspace
            .inputs
            .iter()
            .map(|input| input.definition().clone())
            .chain(
                workspace
                    .formulas
                    .iter()
                    .map(|formula| formula.result.clone()),
            )
            .collect();
        let checked = workspace
            .formulas
            .iter()
            .map(|formula| {
                let parsed = formula::parse(&formula.text);
                let resolved = formula::resolve(&parsed.expression, &definitions, &dimensions);
                let checked = formula::check_axes(&resolved.expression, &formula.result.axes)
                    .checked
                    .expect("the representative formulas are checked");
                (formula.result.clone(), checked)
            })
            .collect();
        Model { workspace, checked }
    }

    fn dimensions(&self) -> Vec<&Dimension> {
        self.workspace.dimensions.iter().collect()
    }

    fn dimension(&self, ulid: DimensionUlid) -> &Dimension {
        self.workspace
            .dimensions
            .iter()
            .find(|dimension| dimension.ulid() == ulid)
            .expect("the representative model has the dimension")
    }

    fn member(&self, dimension: DimensionUlid, key: &str) -> u32 {
        self.dimension(dimension)
            .member_by_key(key)
            .expect("the member exists")
            .id()
            .0
    }

    fn measure(&self, name: &str) -> MeasureUlid {
        self.workspace
            .inputs
            .iter()
            .map(|input| input.definition())
            .chain(
                self.workspace
                    .formulas
                    .iter()
                    .map(|formula| &formula.result),
            )
            .find(|definition| definition.name == name)
            .expect("the representative model has the measure")
            .ulid
    }

    fn changed(
        &self,
        measure: &str,
        changed: &[(DimensionUlid, u32)],
        formula: &str,
    ) -> Option<Region> {
        let (axes, members): (Vec<DimensionUlid>, Vec<MemberId>) = changed
            .iter()
            .map(|&(axis, member)| (axis, MemberId(member)))
            .unzip();
        let region = Region::of_changes(&axes, [members.as_slice()]).expect("one cell changed");
        let changed = HashMap::from([(self.measure(measure), region)]);
        let (_, expression) = self
            .checked
            .iter()
            .find(|(result, _)| result.name == formula)
            .expect("the formula exists");
        column_engine::changed_region(expression, &changed, &self.dimensions())
            .expect("the region is found")
    }

    fn recalculate_all(&self, inputs: Vec<Measure>) -> Vec<Measure> {
        let dimensions = self.dimensions();
        let mut measures = inputs;
        for (result, expression) in &self.checked {
            let references: Vec<&Measure> = measures.iter().collect();
            let measure = column_engine::evaluate(expression, result, &references, &dimensions)
                .expect("the formula evaluates");
            measures.push(measure);
        }
        measures
    }
}

fn only(members: &[u32]) -> Option<BTreeSet<u32>> {
    Some(members.iter().copied().collect())
}

fn assert_region(region: &Region, expected: &[(DimensionUlid, Option<BTreeSet<u32>>)]) {
    for (axis, members) in expected {
        assert_eq!(region.members(*axis), members.as_ref(), "{axis:?}");
    }
}

#[test]
fn a_changed_cell_marks_only_the_cells_that_depend_on_it() {
    let model = Model::new();
    let budget = model.member(VERSION, "予算");
    let actual = model.member(VERSION, "実績");
    let quantity = [
        (DEPARTMENT, 1),
        (PRODUCT, 2),
        (CUSTOMER, 3),
        (PERIOD, 4),
        (VERSION, budget),
    ];

    let sales = model
        .changed("数量", &quantity, "売上")
        .expect("sales change");
    assert_region(
        &sales,
        &[
            (DEPARTMENT, only(&[1])),
            (PRODUCT, only(&[2])),
            (CUSTOMER, only(&[3])),
            (PERIOD, only(&[4])),
            (VERSION, only(&[budget])),
        ],
    );

    let share = model
        .changed("数量", &quantity, "材料費構成比")
        .expect("the share of every product changes");
    assert_region(
        &share,
        &[
            (PRODUCT, None),
            (PERIOD, only(&[4])),
            (VERSION, only(&[budget])),
        ],
    );

    let profit = [
        (DEPARTMENT, 1),
        (PRODUCT, 2),
        (CUSTOMER, 3),
        (PERIOD, 4),
        (VERSION, actual),
    ];
    let variance = model
        .changed("粗利", &profit, "粗利予実差")
        .expect("the variance changes");
    assert_region(
        &variance,
        &[
            (DEPARTMENT, only(&[1])),
            (PERIOD, only(&[4])),
            (VERSION, None),
        ],
    );
    let forecast = model.member(VERSION, "見込");
    let mut unrelated = profit;
    unrelated[4] = (VERSION, forecast);
    assert_eq!(model.changed("粗利", &unrelated, "粗利予実差"), None);

    let selling = model.member(CATEGORY, "販売費及び一般管理費");
    let category = [
        (CATEGORY, selling),
        (DEPARTMENT, 1),
        (PERIOD, 4),
        (VERSION, budget),
    ];
    let operating = model
        .changed("カテゴリ別金額", &category, "営業利益")
        .expect("the operating profit changes");
    assert_region(
        &operating,
        &[
            (CATEGORY, None),
            (DEPARTMENT, only(&[1])),
            (PERIOD, only(&[4])),
        ],
    );

    let accounts = model.dimension(ACCOUNT);
    let account = (0..accounts.len())
        .find(|&account| {
            accounts
                .attributes()
                .next()
                .expect("an account has a group")
                .value(MemberId(account))
                .is_some()
        })
        .expect("some account has a group");
    let group = accounts
        .attributes()
        .next()
        .expect("an account has a group")
        .value(MemberId(account))
        .expect("the account has a group")
        .0;
    let amount = [
        (ACCOUNT, account),
        (DEPARTMENT, 1),
        (PERIOD, 4),
        (VERSION, budget),
    ];
    let grouped = model
        .changed("科目別金額", &amount, "グループ別金額")
        .expect("the group amount changes");
    assert_region(
        &grouped,
        &[
            (GROUP, only(&[group])),
            (ACCOUNT, None),
            (DEPARTMENT, only(&[1])),
        ],
    );
}

#[test]
fn edits_recalculate_to_the_same_cells_as_a_full_recalculation() {
    edit_and_compare(&Model::new());
}

#[test]
fn edits_match_a_full_recalculation_when_results_declare_axes_out_of_key_order() {
    edit_and_compare(&Model::with_results_reversed(true));
}

fn edit_and_compare(model: &Model) {
    let dimensions = model.dimensions();
    let formulas: Vec<(MeasureUlid, &CheckedExpression)> = model
        .checked
        .iter()
        .map(|(result, expression)| (result.ulid, expression))
        .collect();
    let mut inputs = model.workspace.inputs.clone();
    let mut measures = model.recalculate_all(inputs.clone());
    let mut random = StdRng::seed_from_u64(11);
    for _ in 0..40 {
        let input = random.random_range(0..inputs.len());
        let cells: Vec<(Vec<MemberId>, f64)> = inputs[input].cells().collect();
        let coordinate: Vec<MemberId> = match random.random_range(0..3) {
            0 => inputs[input]
                .definition()
                .axes
                .iter()
                .map(|&axis| MemberId(random.random_range(0..model.dimension(axis).len())))
                .collect(),
            _ => cells[random.random_range(0..cells.len())].0.clone(),
        };
        let value =
            (random.random_range(0..4) != 0).then(|| random.random_range(1..1_000) as f64 / 7.0);
        let delta = |measure: &Measure| {
            let mut delta = measure.delta();
            match value {
                Some(value) => delta.set(&coordinate, value),
                None => delta.delete(&coordinate),
            }
            .expect("the coordinate fits");
            delta
        };
        let edited = delta(&measures[input]);
        inputs[input] = inputs[input]
            .apply(delta(&inputs[input]))
            .expect("the delta fits");
        column_engine::edit(
            &mut measures,
            [(inputs[input].ulid(), edited)],
            &formulas,
            &dimensions,
        )
        .expect("the edit recalculates");
        let expected = model.recalculate_all(inputs.clone());
        for (measure, expected) in measures.iter().zip(&expected) {
            assert_eq!(
                exact(&measure.cells().collect::<Vec<_>>()),
                exact(&expected.cells().collect::<Vec<_>>()),
                "{}",
                measure.definition().name
            );
        }
    }
}
