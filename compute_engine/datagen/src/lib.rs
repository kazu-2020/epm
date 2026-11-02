mod formulas;

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::{
    ArrayRef, FixedSizeBinaryArray, Float64Array, RecordBatch, StringArray, UInt32Array,
};
use dimension::{
    AttributeDefinition, AttributeUlid, Dimension, DimensionDefinition, DimensionUlid, KeyKind,
};
use measure::{Measure, MeasureDefinition, MeasureUlid};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use rand_distr::Zipf;
use ulid::Ulid;

const LARGEST_MAGNITUDE: u32 = 10_000;

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

pub struct Workspace {
    pub dimensions: Vec<Dimension>,
    pub inputs: Vec<Measure>,
    pub formulas: Vec<(MeasureDefinition, String)>,
}

pub fn generate(configuration: &Configuration) -> Workspace {
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
    Workspace {
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

    fn skewed(&mut self, len: u32) -> u32 {
        let zipf = Zipf::new(f64::from(len), self.configuration.zipf_exponent)
            .expect("a non-empty range and a non-negative exponent make a Zipf distribution");
        self.random.sample(zipf) as u32 - 1
    }

    fn axes(&mut self, dimensions: &[Dimension]) -> Vec<DimensionUlid> {
        let mut axes: Vec<DimensionUlid> = dimensions.iter().map(Dimension::ulid).collect();
        axes.shuffle(&mut self.random);
        axes.truncate(self.random.random_range(1..=axes.len()));
        axes
    }

    fn dimension(&mut self, index: usize, earlier: &[Dimension]) -> Dimension {
        let ulid = DimensionUlid(self.ulid());
        let len = self
            .random
            .random_range(1..=self.configuration.members_per_dimension);
        let parent = (!earlier.is_empty() && self.random.random_bool(0.75))
            .then(|| &earlier[self.random.random_range(0..earlier.len())]);
        let attributes: Vec<AttributeDefinition> = parent
            .map(|parent| AttributeDefinition {
                ulid: AttributeUlid(self.ulid()),
                name: "親".into(),
                target: parent.ulid(),
            })
            .into_iter()
            .collect();
        let definition = DimensionDefinition {
            ulid,
            name: format!("D{index}"),
            key_kind: KeyKind::Name,
            attributes,
        };
        let ulids = FixedSizeBinaryArray::try_from_iter((0..len).map(|_| self.ulid().to_bytes()))
            .expect("every member ULID is 16 bytes");
        let names = StringArray::from_iter_values((0..len).map(|member| format!("m{member}")));
        let mut columns: Vec<ArrayRef> = vec![Arc::new(ulids), Arc::new(names)];
        if let Some(parent) = parent {
            let targets: UInt32Array = (0..len)
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
            ulid: MeasureUlid(self.ulid()),
            name: format!("入力{index}"),
            axes: self.axes(dimensions),
        };
        let lens: Vec<u32> = definition
            .axes
            .iter()
            .map(|&axis| dimension(dimensions, axis).len())
            .collect();
        let mut cells = BTreeMap::new();
        for _ in 0..self.configuration.cells_per_input {
            let coordinate: Vec<u32> = lens.iter().map(|&len| self.skewed(len)).collect();
            let value = self.value();
            cells.insert(coordinate, value);
        }
        let mut columns: Vec<ArrayRef> = (0..lens.len())
            .map(|axis| {
                Arc::new(UInt32Array::from_iter_values(
                    cells.keys().map(|coordinate| coordinate[axis]),
                )) as ArrayRef
            })
            .collect();
        columns.push(Arc::new(Float64Array::from_iter_values(
            cells.values().copied(),
        )));
        let base = RecordBatch::try_new(Arc::new(definition.schema()), columns)
            .expect("the columns follow the measure schema");
        Measure::try_new(definition, base, &dimensions.iter().collect::<Vec<_>>())
            .expect("generated cells are sorted, unique and normalized")
    }

    fn value(&mut self) -> f64 {
        if self.random.random_bool(0.1) {
            return 0.0;
        }
        let magnitude = f64::from(self.skewed(LARGEST_MAGNITUDE) + 1);
        if self.random.random_bool(0.3) {
            -magnitude
        } else {
            magnitude
        }
    }
}

fn dimension(dimensions: &[Dimension], ulid: DimensionUlid) -> &Dimension {
    dimensions
        .iter()
        .find(|dimension| dimension.ulid() == ulid)
        .expect("generated axes are generated dimensions")
}
