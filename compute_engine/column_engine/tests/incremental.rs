use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::{Dimension, DimensionGlobalId, MemberIndex};
use formula::CheckedExpression;
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use workspace::UncheckedWorkspace;

const SMALL: Size = Size {
    products: 30,
    customers: 20,
    departments_per_kind: 4,
    quantity_cells: 3_000,
    account_fill: 0.5,
};

struct Model {
    workspace: UncheckedWorkspace,
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
                let checked =
                    workspace::compile(&formula.text, &formula.result, &definitions, &dimensions)
                        .expect("the representative formulas are checked");
                (formula.result.clone(), checked)
            })
            .collect();
        Model { workspace, checked }
    }

    fn dimensions(&self) -> Vec<&Dimension> {
        self.workspace.dimensions.iter().collect()
    }

    fn dimension(&self, ulid: DimensionGlobalId) -> &Dimension {
        self.workspace
            .dimensions
            .iter()
            .find(|dimension| dimension.ulid() == ulid)
            .expect("the representative model has the dimension")
    }

    fn measure(&self, name: &str) -> MeasureGlobalId {
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

    fn formulas(&self) -> Vec<(MeasureGlobalId, &CheckedExpression)> {
        self.checked
            .iter()
            .map(|(result, expression)| (result.ulid, expression))
            .collect()
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

#[test]
fn edits_recalculate_to_the_same_cells_as_a_full_recalculation() {
    edit_and_compare(&Model::new());
}

#[test]
fn edits_match_a_full_recalculation_when_results_declare_axes_out_of_packing_order() {
    edit_and_compare(&Model::with_results_reversed(true));
}

fn edit_and_compare(model: &Model) {
    let dimensions = model.dimensions();
    let formulas = model.formulas();
    let mut inputs = model.workspace.inputs.clone();
    let mut measures = model.recalculate_all(inputs.clone());
    let mut random = StdRng::seed_from_u64(11);
    for _ in 0..40 {
        let input = random.random_range(0..inputs.len());
        let cells: Vec<(Vec<MemberIndex>, f64)> = inputs[input].cells().collect();
        let coordinate: Vec<MemberIndex> = match random.random_range(0..3) {
            0 => inputs[input]
                .definition()
                .axes
                .iter()
                .map(|&axis| MemberIndex(random.random_range(0..model.dimension(axis).len())))
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
        measures = column_engine::edit(
            &measures,
            [(inputs[input].ulid(), edited)],
            &formulas,
            &dimensions,
        )
        .expect("the edit recalculates");
        assert_same_cells(&measures, &model.recalculate_all(inputs.clone()));
    }
}

#[test]
fn two_deltas_for_one_measure_recalculate_the_cells_of_both() {
    let model = Model::new();
    let dimensions = model.dimensions();
    let formulas = model.formulas();
    let mut inputs = model.workspace.inputs.clone();
    let mut measures = model.recalculate_all(inputs.clone());
    let quantity = model.measure("数量");
    let input = inputs
        .iter()
        .position(|input| input.ulid() == quantity)
        .expect("the quantity is an input");
    let cells: Vec<(Vec<MemberIndex>, f64)> = inputs[input].cells().collect();
    let (first, last) = (&cells[0].0, &cells[cells.len() - 1].0);
    assert_ne!(first[0], last[0], "the two cells are in different periods");
    let delta = |measure: &Measure, coordinate: &[MemberIndex]| {
        let mut delta = measure.delta();
        delta.set(coordinate, 1_234.5).expect("the coordinate fits");
        delta
    };
    let deltas = [
        (quantity, delta(&measures[input], first)),
        (quantity, delta(&measures[input], last)),
    ];
    measures = column_engine::edit(&measures, deltas, &formulas, &dimensions)
        .expect("the edit recalculates");
    for coordinate in [first, last] {
        inputs[input] = inputs[input]
            .apply(delta(&inputs[input], coordinate))
            .expect("the delta fits");
    }
    assert_same_cells(&measures, &model.recalculate_all(inputs));
}

fn assert_same_cells(measures: &[Measure], expected: &[Measure]) {
    for (measure, expected) in measures.iter().zip(expected) {
        assert_eq!(
            exact(&measure.cells().collect::<Vec<_>>()),
            exact(&expected.cells().collect::<Vec<_>>()),
            "{}",
            measure.definition().name
        );
    }
}
