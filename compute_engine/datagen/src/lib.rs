pub mod cases;
mod formulas;
pub mod representative;

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::{ArrayRef, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, Dimension, DimensionDefinition,
    DimensionGlobalId, DimensionKind, Dimensions, MemberIndex,
};
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use rand_distr::Zipf;
use ulid::Ulid;
use workspace::UncheckedWorkspace;

const LARGEST_MAGNITUDE_IN_CENTS: u32 = 1_000_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Configuration {
    pub seed: u64,
    pub dimension_count: usize,
    pub members_per_dimension: u32,
    pub input_count: usize,
    pub cells_per_input: usize,
    pub zipf_exponent: f64,
    pub chain_length: usize,
    pub maximum_depth: usize,
}

pub fn generate(configuration: &Configuration) -> UncheckedWorkspace {
    assert!(
        configuration.dimension_count > 0 && configuration.members_per_dimension > 0,
        "a workspace needs a dimension with a member"
    );
    let mut generator = Generator {
        configuration,
        random: StdRng::seed_from_u64(configuration.seed),
        last_ulid: 0,
    };
    let mut dimensions = Dimensions::default();
    for index in 0..configuration.dimension_count {
        let dimension = generator.dimension(index, &dimensions);
        dimensions
            .try_push(dimension)
            .expect("every generated dimension has its own ULID");
    }
    let inputs: Vec<Measure> = (0..configuration.input_count)
        .map(|index| generator.input(index, &dimensions))
        .collect();
    let formulas = generator.formulas(&dimensions, &inputs);
    UncheckedWorkspace {
        dimensions,
        inputs,
        formulas,
    }
}

struct Generator<'c> {
    configuration: &'c Configuration,
    random: StdRng,
    last_ulid: u128,
}

impl Generator<'_> {
    fn ulid(&mut self) -> Ulid {
        self.last_ulid += 1;
        Ulid(self.last_ulid)
    }

    fn skewed(&mut self, end: u32) -> u32 {
        let zipf = Zipf::new(f64::from(end), self.configuration.zipf_exponent)
            .expect("a non-empty range and a non-negative exponent make a Zipf distribution");
        zipf_member(self.random.sample(zipf))
    }

    fn axes(&mut self, dimensions: &Dimensions) -> Vec<DimensionGlobalId> {
        let mut axes: Vec<DimensionGlobalId> =
            dimensions.iter().map(Dimension::global_id).collect();
        axes.shuffle(&mut self.random);
        axes.truncate(self.random.random_range(1..=axes.len()));
        axes
    }

    fn dimension(&mut self, index: usize, earlier: &Dimensions) -> Dimension {
        let global_id = DimensionGlobalId(self.ulid());
        let member_count = self
            .random
            .random_range(1..=self.configuration.members_per_dimension);
        let earlier_count = earlier.iter().len();
        let parent = (earlier_count > 0 && self.random.random_bool(0.75)).then(|| {
            earlier
                .iter()
                .nth(self.random.random_range(0..earlier_count))
                .expect("the position is below the count")
        });
        let natural_key = natural_key_attribute(global_id);
        let natural_key_global_id = natural_key.global_id;
        let attributes: Vec<AttributeDefinition> = std::iter::once(natural_key)
            .chain(parent.map(|parent| AttributeDefinition {
                global_id: AttributeGlobalId(self.ulid()),
                name: "親".into(),
                kind: AttributeKind::Reference(parent.global_id()),
            }))
            .collect();
        let definition = DimensionDefinition::try_new(
            global_id,
            format!("D{index}"),
            // Not the ULID order: the last dimensions lead the packed coordinate, so the tests meet packed coordinates in the other order.
            match self.configuration.dimension_count - index {
                1 => DimensionKind::Calendar,
                2 => DimensionKind::Version,
                _ => DimensionKind::Ordinary,
            },
            natural_key_global_id,
            attributes,
        )
        .expect("generated attributes have unique names and ULIDs");
        let global_ids =
            FixedSizeBinaryArray::try_from_iter((0..member_count).map(|_| self.ulid().to_bytes()))
                .expect("every member ULID is 16 bytes");
        let natural_keys =
            StringArray::from_iter_values((0..member_count).map(|member| format!("m{member}")));
        let mut columns: Vec<ArrayRef> = vec![Arc::new(global_ids), Arc::new(natural_keys)];
        if let Some(parent) = parent {
            let targets: UInt32Array = (0..member_count)
                .map(|_| (!self.random.random_bool(0.1)).then(|| self.skewed(parent.len())))
                .collect();
            columns.push(Arc::new(targets));
        }
        let members = RecordBatch::try_new(Arc::new(definition.schema()), columns)
            .expect("the columns follow the dimension schema");
        Dimension::try_new(definition, members, earlier)
            .expect("generated members are unique and their targets exist")
    }

    fn input(&mut self, index: usize, dimensions: &Dimensions) -> Measure {
        let definition = MeasureDefinition::try_new(
            MeasureGlobalId(self.ulid()),
            format!("入力{index}"),
            self.axes(dimensions),
        )
        .expect("the generator declares each axis once");
        let member_counts: Vec<u32> = definition
            .axes()
            .iter()
            .map(|&axis| {
                dimensions
                    .get(axis)
                    .expect("generated axes are generated dimensions")
                    .len()
            })
            .collect();
        let mut cells = BTreeMap::new();
        for _ in 0..self.configuration.cells_per_input {
            let coordinate: Vec<MemberIndex> = member_counts
                .iter()
                .map(|&member_count| MemberIndex(self.skewed(member_count)))
                .collect();
            let value = self.value();
            cells.insert(coordinate, value);
        }
        Measure::from_cells(definition, cells.into_iter().collect(), dimensions)
            .expect("generated cells are unique and normalized")
    }

    fn value(&mut self) -> f64 {
        if self.random.random_bool(0.1) {
            return 0.0;
        }
        let magnitude = f64::from(self.skewed(LARGEST_MAGNITUDE_IN_CENTS) + 1) / 100.0;
        if self.random.random_bool(0.3) {
            -magnitude
        } else {
            magnitude
        }
    }
}

// Not a random ULID: drawing one more would change every model that an existing seed generates.
pub(crate) fn natural_key_attribute(dimension: DimensionGlobalId) -> AttributeDefinition {
    AttributeDefinition {
        global_id: AttributeGlobalId(dimension.0),
        name: "ID".into(),
        kind: AttributeKind::Text,
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a Zipf sample is a whole number from 1 to its u32 end, and f64 has no TryFrom to u32"
)]
pub(crate) fn zipf_member(sample: f64) -> u32 {
    sample as u32 - 1
}

pub fn numbered_dimension(
    id: u128,
    members: u32,
    kind: DimensionKind,
    targets: &Dimensions,
) -> Dimension {
    let global_id = DimensionGlobalId(Ulid(id));
    let natural_key = natural_key_attribute(global_id);
    let natural_key_global_id = natural_key.global_id;
    let attributes = std::iter::once(natural_key)
        .chain(
            (0u128..)
                .zip(targets)
                .map(|(position, target)| AttributeDefinition {
                    global_id: AttributeGlobalId(Ulid(id << 64 | (position + 1))),
                    name: target.name().into(),
                    kind: AttributeKind::Reference(target.global_id()),
                }),
        )
        .collect();
    let definition = DimensionDefinition::try_new(
        global_id,
        format!("D{id}"),
        kind,
        natural_key_global_id,
        attributes,
    )
    .expect("the attributes are named after distinct targets");
    let global_ids = FixedSizeBinaryArray::try_from_iter(
        (0..members).map(|member| Ulid(id << 64 | u128::from(member)).to_bytes()),
    )
    .expect("every member ULID is 16 bytes");
    let natural_keys = StringArray::from_iter_values((0..members).map(|member| member.to_string()));
    let named: [ArrayRef; 2] = [Arc::new(global_ids), Arc::new(natural_keys)];
    let columns: Vec<ArrayRef> = named
        .into_iter()
        .chain(targets.iter().map(|target| -> ArrayRef {
            Arc::new(
                (0..members)
                    .map(|member| member.checked_rem(target.len()))
                    .collect::<UInt32Array>(),
            )
        }))
        .collect();
    let members = RecordBatch::try_new(Arc::new(definition.schema()), columns)
        .expect("the columns follow the dimension schema");
    Dimension::try_new(definition, members, targets).expect("every target member exists")
}
