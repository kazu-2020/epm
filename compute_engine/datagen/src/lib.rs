pub mod cases;
mod formulas;
pub mod representative;

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::{ArrayRef, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, Dimension, DimensionDefinition,
    DimensionGlobalId, DimensionKind, MemberIndex,
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
    let mut dimensions = Vec::with_capacity(configuration.dimension_count);
    for index in 0..configuration.dimension_count {
        let dimension = generator.dimension(index, &dimensions);
        dimensions.push(dimension);
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
        self.random.sample(zipf) as u32 - 1
    }

    fn axes(&mut self, dimensions: &[Dimension]) -> Vec<DimensionGlobalId> {
        let mut axes: Vec<DimensionGlobalId> = dimensions.iter().map(Dimension::ulid).collect();
        axes.shuffle(&mut self.random);
        axes.truncate(self.random.random_range(1..=axes.len()));
        axes
    }

    fn dimension(&mut self, index: usize, earlier: &[Dimension]) -> Dimension {
        let ulid = DimensionGlobalId(self.ulid());
        let member_count = self
            .random
            .random_range(1..=self.configuration.members_per_dimension);
        let parent = (!earlier.is_empty() && self.random.random_bool(0.75))
            .then(|| &earlier[self.random.random_range(0..earlier.len())]);
        let natural_key = natural_key_attribute(ulid);
        let natural_key_ulid = natural_key.ulid;
        let attributes: Vec<AttributeDefinition> = std::iter::once(natural_key)
            .chain(parent.map(|parent| AttributeDefinition {
                ulid: AttributeGlobalId(self.ulid()),
                name: "親".into(),
                kind: AttributeKind::Reference(parent.ulid()),
            }))
            .collect();
        let definition = DimensionDefinition::try_new(
            ulid,
            format!("D{index}"),
            // Not the ULID order: the last dimensions lead the packed coordinate, so the tests meet packed coordinates in the other order.
            match self.configuration.dimension_count - index {
                1 => DimensionKind::Calendar,
                2 => DimensionKind::Version,
                _ => DimensionKind::Ordinary,
            },
            natural_key_ulid,
            attributes,
        )
        .expect("generated attributes have unique names and ULIDs");
        let ulids =
            FixedSizeBinaryArray::try_from_iter((0..member_count).map(|_| self.ulid().to_bytes()))
                .expect("every member ULID is 16 bytes");
        let natural_keys =
            StringArray::from_iter_values((0..member_count).map(|member| format!("m{member}")));
        let mut columns: Vec<ArrayRef> = vec![Arc::new(ulids), Arc::new(natural_keys)];
        if let Some(parent) = parent {
            let targets: UInt32Array = (0..member_count)
                .map(|_| (!self.random.random_bool(0.1)).then(|| self.skewed(parent.len())))
                .collect();
            columns.push(Arc::new(targets));
        }
        let members = RecordBatch::try_new(Arc::new(definition.schema()), columns)
            .expect("the columns follow the dimension schema");
        Dimension::try_new(definition, members, &earlier.iter().collect::<Vec<_>>())
            .expect("generated members are unique and their targets exist")
    }

    fn input(&mut self, index: usize, dimensions: &[Dimension]) -> Measure {
        let definition = MeasureDefinition {
            ulid: MeasureGlobalId(self.ulid()),
            name: format!("入力{index}"),
            axes: self.axes(dimensions),
        };
        let member_counts: Vec<u32> = definition
            .axes
            .iter()
            .map(|&axis| dimension(dimensions, axis).len())
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
        Measure::from_cells(
            definition,
            cells.into_iter().collect(),
            &dimensions.iter().collect::<Vec<_>>(),
        )
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

fn dimension(dimensions: &[Dimension], ulid: DimensionGlobalId) -> &Dimension {
    dimensions
        .iter()
        .find(|dimension| dimension.ulid() == ulid)
        .expect("generated axes are generated dimensions")
}

// Not a random ULID: drawing one more would change every model that an existing seed generates.
pub(crate) fn natural_key_attribute(dimension: DimensionGlobalId) -> AttributeDefinition {
    AttributeDefinition {
        ulid: AttributeGlobalId(dimension.0),
        name: "ID".into(),
        kind: AttributeKind::Text,
    }
}
