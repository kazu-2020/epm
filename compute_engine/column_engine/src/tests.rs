use std::collections::{BTreeSet, HashMap};

use datagen::representative::{self, Size};
use dimension::{Dimension, DimensionGlobalId, MemberIndex};
use ulid::Ulid;
use workspace::Workspace;

use crate::region::Region;

const SMALL: Size = Size {
    products: 30,
    customers: 20,
    departments_per_kind: 4,
    quantity_cells: 3_000,
    account_fill: 0.5,
};

const CATEGORY: DimensionGlobalId = DimensionGlobalId(Ulid(1));
const GROUP: DimensionGlobalId = DimensionGlobalId(Ulid(2));
const ACCOUNT: DimensionGlobalId = DimensionGlobalId(Ulid(3));
const DEPARTMENT: DimensionGlobalId = DimensionGlobalId(Ulid(5));
const PRODUCT: DimensionGlobalId = DimensionGlobalId(Ulid(7));
const CUSTOMER: DimensionGlobalId = DimensionGlobalId(Ulid(8));
const PERIOD: DimensionGlobalId = DimensionGlobalId(Ulid(9));
const VERSION: DimensionGlobalId = DimensionGlobalId(Ulid(10));

struct Model {
    workspace: Workspace,
}

impl Model {
    fn new() -> Model {
        Model {
            workspace: Workspace::try_new(representative::generate(&SMALL, 7))
                .expect("the representative model is valid"),
        }
    }

    fn dimension(&self, global_id: DimensionGlobalId) -> &Dimension {
        self.workspace
            .dimensions()
            .iter()
            .find(|dimension| dimension.global_id() == global_id)
            .expect("the representative model has the dimension")
    }

    fn member(&self, dimension: DimensionGlobalId, natural_key: &str) -> u32 {
        self.dimension(dimension)
            .member_by_natural_key(natural_key)
            .expect("the member exists")
            .id()
            .0
    }

    fn changed(
        &self,
        measure: &str,
        changed: &[(DimensionGlobalId, u32)],
        formula: &str,
    ) -> Option<Region> {
        let (axes, members): (Vec<DimensionGlobalId>, Vec<u32>) = changed.iter().copied().unzip();
        let region = Region::of_changes(&axes, [members.as_slice()]).expect("one cell changed");
        let definitions = self
            .workspace
            .inputs()
            .iter()
            .map(|input| input.definition());
        let results = self
            .workspace
            .formulas()
            .iter()
            .map(|formula| formula.result());
        let measure = definitions
            .chain(results)
            .find(|definition| definition.name == measure)
            .expect("the representative model has the measure")
            .global_id;
        let changed = HashMap::from([(measure, region)]);
        let formula = self
            .workspace
            .formulas()
            .iter()
            .find(|candidate| candidate.result().name == formula)
            .expect("the formula exists");
        let dimensions: Vec<&Dimension> = self.workspace.dimensions().iter().collect();
        crate::changed_region(formula.expression(), &changed, &dimensions)
            .expect("the region is found")
    }
}

fn only(members: &[u32]) -> Option<BTreeSet<u32>> {
    Some(members.iter().copied().collect())
}

fn assert_region(region: &Region, expected: &[(DimensionGlobalId, Option<BTreeSet<u32>>)]) {
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
                .reference_attributes()
                .next()
                .expect("an account has a group")
                .value(MemberIndex(account))
                .is_some()
        })
        .expect("some account has a group");
    let group = accounts
        .reference_attributes()
        .next()
        .expect("an account has a group")
        .value(MemberIndex(account))
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
