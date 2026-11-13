use std::collections::HashSet;
use std::sync::Arc;

use crate::natural_key_attribute;
use arrow_array::{
    ArrayRef, FixedSizeBinaryArray, Float64Array, RecordBatch, StringArray, UInt32Array,
};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, Dimension, DimensionDefinition,
    DimensionGlobalId, DimensionKind,
};
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rand_distr::Zipf;
use rayon::prelude::*;
use ulid::Ulid;
use workspace::{Formula, Workspace};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub products: u32,
    pub customers: u32,
    pub departments_per_kind: u32,
    pub quantity_cells: usize,
    pub account_fill: f64,
}

impl Size {
    pub const FULL: Size = Size {
        products: 1_500,
        customers: 400,
        departments_per_kind: 100,
        quantity_cells: 17_500_000,
        account_fill: 0.5,
    };

    pub const TINY: Size = Size {
        products: 2,
        customers: 2,
        departments_per_kind: 1,
        quantity_cells: 50,
        account_fill: 0.02,
    };
}

const CATEGORY: DimensionGlobalId = DimensionGlobalId(Ulid(1));
const GROUP: DimensionGlobalId = DimensionGlobalId(Ulid(2));
const ACCOUNT: DimensionGlobalId = DimensionGlobalId(Ulid(3));
const DIVISION: DimensionGlobalId = DimensionGlobalId(Ulid(4));
const DEPARTMENT: DimensionGlobalId = DimensionGlobalId(Ulid(5));
const PRODUCT_CATEGORY: DimensionGlobalId = DimensionGlobalId(Ulid(6));
const PRODUCT: DimensionGlobalId = DimensionGlobalId(Ulid(7));
const CUSTOMER: DimensionGlobalId = DimensionGlobalId(Ulid(8));
const PERIOD: DimensionGlobalId = DimensionGlobalId(Ulid(9));
const VERSION: DimensionGlobalId = DimensionGlobalId(Ulid(10));

const PERIODS: u32 = 36;
const VERSIONS: [&str; 3] = ["実績", "予算", "見込"];
const DIVISIONS: u32 = 12;
const PRODUCT_CATEGORIES: u32 = 40;

const CATEGORIES: [(&str, &[(&str, u32)]); 7] = [
    (
        "売上高",
        &[("製品売上", 6), ("商品売上", 4), ("売上値引戻り", 5)],
    ),
    (
        "売上原価",
        &[
            ("材料費", 15),
            ("労務費", 12),
            ("外注加工費", 8),
            ("製造経費", 30),
        ],
    ),
    (
        "販売費及び一般管理費",
        &[
            ("人件費", 20),
            ("販売促進費", 15),
            ("物流費", 10),
            ("減価償却費", 5),
            ("地代家賃・設備費", 12),
            ("研究開発費", 10),
            ("一般管理費", 35),
        ],
    ),
    ("営業外収益", &[("受取利息配当金", 5), ("雑収入", 5)]),
    ("営業外費用", &[("支払利息", 3), ("雑損失", 3)]),
    ("特別利益", &[("固定資産売却益", 3)]),
    ("特別損失", &[("固定資産除却損", 3)]),
];

const MANUFACTURING_GROUPS: [&str; 3] = ["労務費", "外注加工費", "製造経費"];
const FROM_QUANTITY_GROUPS: [&str; 4] = ["製品売上", "商品売上", "売上値引戻り", "材料費"];

#[derive(Clone, Copy, PartialEq)]
enum DepartmentKind {
    Manufacturing,
    Sales,
    Administration,
}

const DEPARTMENT_KINDS: [DepartmentKind; 3] = [
    DepartmentKind::Manufacturing,
    DepartmentKind::Sales,
    DepartmentKind::Administration,
];

const FORMULAS: [(&str, &[DimensionGlobalId], &str); 15] = [
    (
        "売上",
        &[PERIOD, VERSION, DEPARTMENT, PRODUCT, CUSTOMER],
        "'数量' * '単価'",
    ),
    (
        "グループ別金額",
        &[PERIOD, VERSION, GROUP, DEPARTMENT],
        "SUM('科目別金額', BY '勘定科目'.'グループ', '部門', '期間', 'バージョン')",
    ),
    (
        "カテゴリ別金額",
        &[PERIOD, VERSION, CATEGORY, DEPARTMENT],
        "SUM('グループ別金額', BY '勘定科目グループ'.'カテゴリ', '部門', '期間', 'バージョン')",
    ),
    (
        "製造間接費",
        &[PERIOD, VERSION],
        "SUM(FILTER('グループ別金額', '勘定科目グループ' IN ('労務費', '外注加工費', '製造経費')), BY '期間', 'バージョン')",
    ),
    (
        "材料費構成比",
        &[PERIOD, VERSION, PRODUCT],
        "SUM('数量' * '単位材料費' / '歩留率', BY '商品', '期間', 'バージョン') / SUM('数量' * '単位材料費' / '歩留率', BY '期間', 'バージョン')",
    ),
    (
        "単位間接費",
        &[PERIOD, VERSION, PRODUCT],
        "'製造間接費' * '材料費構成比' / SUM('数量', BY '商品', '期間', 'バージョン')",
    ),
    (
        "原価",
        &[PERIOD, VERSION, DEPARTMENT, PRODUCT, CUSTOMER],
        "'数量' * ('単位材料費' / '歩留率' + '単位間接費')",
    ),
    (
        "粗利",
        &[PERIOD, VERSION, DEPARTMENT, PRODUCT, CUSTOMER],
        "'売上' - '原価'",
    ),
    (
        "部門別粗利",
        &[PERIOD, VERSION, DEPARTMENT],
        "SUM('粗利', BY '部門', '期間', 'バージョン')",
    ),
    (
        "営業利益",
        &[PERIOD, VERSION, DEPARTMENT],
        "'部門別粗利' - 'カテゴリ別金額'['勘定科目カテゴリ': '販売費及び一般管理費']",
    ),
    (
        "経常利益",
        &[PERIOD, VERSION, DEPARTMENT],
        "'営業利益' + 'カテゴリ別金額'['勘定科目カテゴリ': '営業外収益'] - 'カテゴリ別金額'['勘定科目カテゴリ': '営業外費用']",
    ),
    (
        "税引前利益",
        &[PERIOD, VERSION, DEPARTMENT],
        "'経常利益' + 'カテゴリ別金額'['勘定科目カテゴリ': '特別利益'] - 'カテゴリ別金額'['勘定科目カテゴリ': '特別損失']",
    ),
    (
        "粗利予実差",
        &[PERIOD, DEPARTMENT, PRODUCT, CUSTOMER],
        "'粗利'['バージョン': '実績'] - '粗利'['バージョン': '予算']",
    ),
    (
        "営業利益予実差",
        &[PERIOD, DEPARTMENT],
        "'営業利益'['バージョン': '実績'] - '営業利益'['バージョン': '予算']",
    ),
    (
        "商品構成比",
        &[PERIOD, VERSION, DEPARTMENT, PRODUCT, CUSTOMER],
        "'売上' / SUM('売上', BY '期間', 'バージョン')",
    ),
];

pub fn generate(size: &Size, seed: u64) -> Workspace {
    let mut generator = Representative {
        size: *size,
        random: StdRng::seed_from_u64(seed),
    };
    let dimensions = generator.dimensions();
    let references: Vec<&Dimension> = dimensions.iter().collect();
    let inputs = generator.inputs(&dimensions, &references);
    Workspace {
        dimensions,
        inputs,
        formulas: formulas(),
    }
}

pub fn dimensions(size: &Size, seed: u64) -> Vec<Dimension> {
    Representative {
        size: *size,
        random: StdRng::seed_from_u64(seed),
    }
    .dimensions()
}

pub fn formulas() -> Vec<Formula> {
    FORMULAS
        .iter()
        .enumerate()
        .map(|(index, &(name, axes, text))| Formula {
            result: MeasureDefinition {
                ulid: MeasureGlobalId(Ulid(200 + index as u128)),
                name: name.into(),
                axes: axes.to_vec(),
            },
            text: text.into(),
        })
        .collect()
}

fn zipf(len: u32) -> Zipf<f64> {
    Zipf::new(f64::from(len), 1.1).expect("a non-empty range makes a Zipf distribution")
}

struct Representative {
    size: Size,
    random: StdRng,
}

fn attribute(ulid: u128, name: &str, target: DimensionGlobalId) -> AttributeDefinition {
    AttributeDefinition {
        ulid: AttributeGlobalId(Ulid(ulid)),
        name: name.into(),
        kind: AttributeKind::Reference(target),
    }
}

fn dimension(
    ulid: DimensionGlobalId,
    name: &str,
    natural_keys: Vec<String>,
    attribute: Option<(AttributeDefinition, Vec<u32>)>,
    earlier: &[Dimension],
) -> Dimension {
    let (attributes, targets) = attribute.unzip();
    let definition = DimensionDefinition::try_new(
        ulid,
        name.into(),
        match ulid {
            PERIOD => DimensionKind::Calendar,
            VERSION => DimensionKind::Version,
            _ => DimensionKind::Ordinary,
        },
        natural_key_attribute(ulid).ulid,
        std::iter::once(natural_key_attribute(ulid))
            .chain(attributes)
            .collect(),
    )
    .expect("the representative dimension definitions are valid");
    let first_member = ulid.0.0 << 32;
    let ulids = FixedSizeBinaryArray::try_from_iter(
        (0..natural_keys.len()).map(|row| Ulid(first_member + row as u128).to_bytes()),
    )
    .expect("every member ULID is 16 bytes");
    let mut columns: Vec<ArrayRef> =
        vec![Arc::new(ulids), Arc::new(StringArray::from(natural_keys))];
    if let Some(targets) = targets {
        columns.push(Arc::new(UInt32Array::from(targets)));
    }
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns)
        .expect("the columns follow the dimension schema");
    Dimension::try_new(definition, batch, &earlier.iter().collect::<Vec<_>>())
        .expect("the representative dimensions are valid")
}

fn numbered(prefix: &str, member_count: u32) -> Vec<String> {
    (1..=member_count)
        .map(|number| format!("{prefix}{number}"))
        .collect()
}

fn cents(cents: u32) -> f64 {
    f64::from(cents) / 100.0
}

impl Representative {
    fn department_kind(&self, department: u32) -> DepartmentKind {
        DEPARTMENT_KINDS[(department / self.size.departments_per_kind) as usize]
    }

    fn departments(&self) -> u32 {
        self.size.departments_per_kind * DEPARTMENT_KINDS.len() as u32
    }

    fn dimensions(&mut self) -> Vec<Dimension> {
        let mut dimensions: Vec<Dimension> = Vec::new();
        let category_names = CATEGORIES
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        dimensions.push(dimension(
            CATEGORY,
            "勘定科目カテゴリ",
            category_names,
            None,
            &dimensions,
        ));
        let mut group_names = Vec::new();
        let mut group_categories = Vec::new();
        let mut account_names = Vec::new();
        let mut account_groups = Vec::new();
        for (category, (_, groups)) in CATEGORIES.iter().enumerate() {
            for &(group, accounts) in *groups {
                for number in 1..=accounts {
                    account_names.push(format!("{group}{number}"));
                    account_groups.push(group_names.len() as u32);
                }
                group_names.push(group.to_string());
                group_categories.push(category as u32);
            }
        }
        dimensions.push(dimension(
            GROUP,
            "勘定科目グループ",
            group_names,
            Some((attribute(101, "カテゴリ", CATEGORY), group_categories)),
            &dimensions,
        ));
        dimensions.push(dimension(
            ACCOUNT,
            "勘定科目",
            account_names,
            Some((attribute(102, "グループ", GROUP), account_groups)),
            &dimensions,
        ));
        dimensions.push(dimension(
            DIVISION,
            "事業部",
            numbered("事業部", DIVISIONS),
            None,
            &dimensions,
        ));
        let departments = self.departments();
        dimensions.push(dimension(
            DEPARTMENT,
            "部門",
            numbered("部門", departments),
            Some((
                attribute(103, "事業部", DIVISION),
                (0..departments)
                    .map(|department| department % DIVISIONS)
                    .collect(),
            )),
            &dimensions,
        ));
        dimensions.push(dimension(
            PRODUCT_CATEGORY,
            "商品カテゴリ",
            numbered("商品カテゴリ", PRODUCT_CATEGORIES),
            None,
            &dimensions,
        ));
        let products = self.size.products;
        let product_categories = (0..products)
            .map(|_| self.random.random_range(0..PRODUCT_CATEGORIES))
            .collect();
        dimensions.push(dimension(
            PRODUCT,
            "商品",
            numbered("商品", products),
            Some((
                attribute(104, "カテゴリ", PRODUCT_CATEGORY),
                product_categories,
            )),
            &dimensions,
        ));
        dimensions.push(dimension(
            CUSTOMER,
            "取引先",
            numbered("取引先", self.size.customers),
            None,
            &dimensions,
        ));
        dimensions.push(dimension(
            PERIOD,
            "期間",
            (0..PERIODS)
                .map(|month| format!("{}年{}月", 2025 + month / 12, month % 12 + 1))
                .collect(),
            None,
            &dimensions,
        ));
        dimensions.push(dimension(
            VERSION,
            "バージョン",
            VERSIONS.iter().map(|name| name.to_string()).collect(),
            None,
            &dimensions,
        ));
        dimensions
    }

    fn inputs(&mut self, dimensions: &[Dimension], references: &[&Dimension]) -> Vec<Measure> {
        let product_axes = [PERIOD, VERSION, PRODUCT];
        let prices = self.dense(&product_axes, |random| {
            cents(random.random_range(10_000..=5_000_000))
        });
        let unit_material_costs = prices
            .iter()
            .map(|&price| (price * self.random.random_range(30..=60) as f64).round() / 100.0)
            .collect();
        let yields = self.dense(&product_axes, |random| cents(random.random_range(85..=100)));
        let product_columns = self.dense_columns(&product_axes);
        let accounts = dimensions
            .iter()
            .find(|dimension| dimension.ulid() == ACCOUNT)
            .expect("the accounts are generated");
        vec![
            self.quantities(references),
            measure(
                1,
                "単価",
                &product_axes,
                product_columns.clone(),
                prices,
                references,
            ),
            measure(
                2,
                "単位材料費",
                &product_axes,
                product_columns.clone(),
                unit_material_costs,
                references,
            ),
            measure(
                3,
                "歩留率",
                &product_axes,
                product_columns,
                yields,
                references,
            ),
            self.account_amounts(accounts, dimensions, references),
        ]
    }

    fn member_counts(&self, axes: &[DimensionGlobalId]) -> Vec<u32> {
        axes.iter()
            .map(|&axis| match axis {
                PRODUCT => self.size.products,
                CUSTOMER => self.size.customers,
                DEPARTMENT => self.departments(),
                PERIOD => PERIODS,
                VERSION => VERSIONS.len() as u32,
                _ => unreachable!("only these axes are filled densely"),
            })
            .collect()
    }

    fn dense_columns(&self, axes: &[DimensionGlobalId]) -> Vec<Vec<u32>> {
        let member_counts = self.member_counts(axes);
        let cells: u32 = member_counts.iter().product();
        let mut columns = vec![Vec::with_capacity(cells as usize); axes.len()];
        for cell in 0..cells {
            let mut rest = cell;
            for (column, &member_count) in columns.iter_mut().zip(&member_counts).rev() {
                column.push(rest % member_count);
                rest /= member_count;
            }
        }
        columns
    }

    fn dense(
        &mut self,
        axes: &[DimensionGlobalId],
        value: impl Fn(&mut StdRng) -> f64,
    ) -> Vec<f64> {
        let cells: u32 = self.member_counts(axes).iter().product();
        (0..cells).map(|_| value(&mut self.random)).collect()
    }

    fn quantities(&mut self, references: &[&Dimension]) -> Measure {
        let axes = [PERIOD, VERSION, DEPARTMENT, PRODUCT, CUSTOMER];
        let member_counts = self.member_counts(&axes);
        let sales_departments = self.size.departments_per_kind;
        let [periods, versions, _, products, customers] = member_counts[..] else {
            unreachable!("five axes have five member counts")
        };
        let possible = [sales_departments, periods, versions, products, customers]
            .iter()
            .map(|&member_count| u64::from(member_count))
            .product::<u64>();
        assert!(
            (self.size.quantity_cells as u64) < possible / 2,
            "the quantity cells fit sparsely into the sales departments"
        );
        let pairs = periods * versions;
        let seeds: Vec<u64> = (0..pairs).map(|_| self.random.random()).collect();
        let [department_zipf, product_zipf, customer_zipf] =
            [sales_departments, products, customers].map(zipf);
        let value_zipf = zipf(500);
        let (cell_indexes, values): (Vec<Vec<u64>>, Vec<Vec<f64>>) = seeds
            .into_par_iter()
            .enumerate()
            .map(|(pair, seed)| {
                let mut random = StdRng::seed_from_u64(seed);
                let pair = pair as u32;
                let (period, version) = (pair / versions, pair % versions);
                let cells = self.size.quantity_cells / pairs as usize
                    + usize::from((pair as usize) < self.size.quantity_cells % pairs as usize);
                let mut seen: HashSet<u64> = HashSet::with_capacity(cells);
                while seen.len() < cells {
                    let department = sales_departments + random.sample(department_zipf) as u32 - 1;
                    let product = random.sample(product_zipf) as u32 - 1;
                    let customer = random.sample(customer_zipf) as u32 - 1;
                    seen.insert(cell_index(
                        &[period, version, department, product, customer],
                        &member_counts,
                    ));
                }
                let mut cell_indexes: Vec<u64> = seen.into_iter().collect();
                cell_indexes.sort_unstable();
                let values = (0..cell_indexes.len())
                    .map(|_| random.sample(value_zipf))
                    .collect();
                (cell_indexes, values)
            })
            .unzip();
        let cell_indexes: Vec<u64> = cell_indexes.into_iter().flatten().collect();
        let values: Vec<f64> = values.into_iter().flatten().collect();
        debug_assert!(cell_indexes.is_sorted());
        let mut columns = vec![Vec::with_capacity(cell_indexes.len()); axes.len()];
        for &cell_index in &cell_indexes {
            for (column, member) in columns
                .iter_mut()
                .zip(coordinate_of(cell_index, &member_counts))
            {
                column.push(member);
            }
        }
        let [_, _, departments, _, _] = &columns[..] else {
            unreachable!("five axes have five columns")
        };
        debug_assert!(
            departments
                .iter()
                .all(|&department| { self.department_kind(department) == DepartmentKind::Sales })
        );
        measure(0, "数量", &axes, columns, values, references)
    }

    fn account_amounts(
        &mut self,
        accounts: &Dimension,
        dimensions: &[Dimension],
        references: &[&Dimension],
    ) -> Measure {
        let groups = dimensions
            .iter()
            .find(|dimension| dimension.ulid() == GROUP)
            .expect("the groups are generated");
        let categories = dimensions
            .iter()
            .find(|dimension| dimension.ulid() == CATEGORY)
            .expect("the categories are generated");
        let group_of = accounts
            .reference_attribute("グループ")
            .expect("accounts have a group");
        let category_of = groups
            .reference_attribute("カテゴリ")
            .expect("groups have a category");
        let mut pairs = Vec::new();
        for account in 0..accounts.len() {
            let group = group_of
                .value(dimension::MemberIndex(account))
                .expect("every account has a group");
            let group_natural_key = groups
                .member(group)
                .expect("the group exists")
                .natural_key();
            if FROM_QUANTITY_GROUPS.contains(&group_natural_key) {
                continue;
            }
            let category = category_of
                .value(group)
                .expect("every group has a category");
            let category_natural_key = categories
                .member(category)
                .expect("the category exists")
                .natural_key();
            for department in 0..self.departments() {
                let kind = self.department_kind(department);
                let books = if MANUFACTURING_GROUPS.contains(&group_natural_key) {
                    kind == DepartmentKind::Manufacturing
                } else if category_natural_key == "販売費及び一般管理費" {
                    true
                } else {
                    kind == DepartmentKind::Administration
                };
                if books && self.random.random_bool(self.size.account_fill) {
                    pairs.push((account, department));
                }
            }
        }
        let axes = [PERIOD, VERSION, ACCOUNT, DEPARTMENT];
        let mut columns = vec![Vec::new(); axes.len()];
        let mut values = Vec::new();
        for period in 0..PERIODS {
            for version in 0..VERSIONS.len() as u32 {
                for &(account, department) in &pairs {
                    for (column, member) in columns
                        .iter_mut()
                        .zip([period, version, account, department])
                    {
                        column.push(member);
                    }
                    values.push(cents(self.random.random_range(100_000..=100_000_000)));
                }
            }
        }
        measure(4, "科目別金額", &axes, columns, values, references)
    }
}

fn cell_index(coordinate: &[u32], member_counts: &[u32]) -> u64 {
    coordinate
        .iter()
        .zip(member_counts)
        .fold(0, |index, (&member, &member_count)| {
            index * u64::from(member_count) + u64::from(member)
        })
}

fn coordinate_of(mut index: u64, member_counts: &[u32]) -> Vec<u32> {
    let mut coordinate = vec![0; member_counts.len()];
    for (member, &member_count) in coordinate.iter_mut().zip(member_counts).rev() {
        *member = (index % u64::from(member_count)) as u32;
        index /= u64::from(member_count);
    }
    coordinate
}

fn measure(
    index: u128,
    name: &str,
    axes: &[DimensionGlobalId],
    columns: Vec<Vec<u32>>,
    values: Vec<f64>,
    references: &[&Dimension],
) -> Measure {
    let definition = MeasureDefinition {
        ulid: MeasureGlobalId(Ulid(100 + index)),
        name: name.into(),
        axes: axes.to_vec(),
    };
    let mut arrays: Vec<ArrayRef> = columns
        .into_iter()
        .map(|column| Arc::new(UInt32Array::from(column)) as ArrayRef)
        .collect();
    arrays.push(Arc::new(Float64Array::from(values)));
    let base = RecordBatch::try_new(Arc::new(definition.schema()), arrays)
        .expect("the columns follow the measure schema");
    Measure::try_new(definition, base, references).expect("the representative cells are valid")
}
