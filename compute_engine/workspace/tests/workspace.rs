use std::collections::BTreeMap;

use datagen::cases::exact;
use datagen::numbered_dimension;
use datagen::representative::{self, Size};
use dimension::{AttributeGlobalId, Dimension, DimensionGlobalId, DimensionKind, MemberIndex};
use formula::{AxisErrorKind, EvaluateError, ResolveErrorKind};
use measure::{Measure, MeasureDefinition, MeasureGlobalId};
use ulid::Ulid;
use workspace::{
    Calculated, CellError, Coordinate, Edit, EditError, Edits, Formula, FormulaError,
    InvalidWorkspaceGlobalId, UncheckedWorkspace, Workspace, WorkspaceError, WorkspaceGlobalId,
    resolve,
};

const QUANTITY: MeasureGlobalId = MeasureGlobalId(Ulid(100));
const SALES: MeasureGlobalId = MeasureGlobalId(Ulid(200));
const OVERHEAD: MeasureGlobalId = MeasureGlobalId(Ulid(203));
const COST: MeasureGlobalId = MeasureGlobalId(Ulid(206));
const GROSS_PROFIT: MeasureGlobalId = MeasureGlobalId(Ulid(207));
const PERIOD: DimensionGlobalId = DimensionGlobalId(Ulid(9));
const PERIOD_OTHER: DimensionGlobalId = DimensionGlobalId(Ulid(0xdead));

fn generated() -> UncheckedWorkspace {
    representative::generate(&Size::TINY, 7)
}

fn calculated(unchecked: UncheckedWorkspace) -> Calculated {
    let workspace = Workspace::try_new(unchecked).expect("the representative model is valid");
    Calculated::new(workspace).expect("the representative formulas evaluate")
}

type Cells = Vec<(Vec<u32>, String)>;

fn cells(calculated: &Calculated) -> BTreeMap<MeasureGlobalId, Cells> {
    calculated
        .measures()
        .iter()
        .map(|measure| {
            (
                measure.global_id(),
                exact(&measure.cells().collect::<Vec<_>>()),
            )
        })
        .collect()
}

fn error(change: impl FnOnce(&mut UncheckedWorkspace)) -> WorkspaceError {
    let mut unchecked = generated();
    change(&mut unchecked);
    match Workspace::try_new(unchecked) {
        Ok(_) => panic!("the change breaks the workspace"),
        Err(error) => error,
    }
}

fn formula(unchecked: &mut UncheckedWorkspace, result: MeasureGlobalId) -> &mut Formula {
    unchecked
        .formulas
        .iter_mut()
        .find(|formula| formula.result.global_id() == result)
        .expect("the representative model has the formula")
}

fn quantity_cell() -> [MemberIndex; 5] {
    [0, 1, 1, 0, 1].map(MemberIndex)
}

fn named(
    unchecked: &UncheckedWorkspace,
    axes: &[DimensionGlobalId],
    members: &[MemberIndex],
) -> Coordinate {
    axes.iter()
        .zip(members)
        .map(|(&axis, &member)| {
            let dimension = unchecked
                .dimensions
                .iter()
                .find(|dimension| dimension.global_id() == axis)
                .expect("the axis is a dimension");
            (
                axis,
                dimension
                    .member(member)
                    .expect("the member exists")
                    .global_id(),
            )
        })
        .collect()
}

fn quantity(value: Option<f64>) -> Edit {
    let unchecked = generated();
    let input = &unchecked.inputs[0];
    assert_eq!(
        input.global_id(),
        QUANTITY,
        "the first input is the quantity"
    );
    Edit {
        measure: QUANTITY,
        coordinate: named(&unchecked, input.definition().axes(), &quantity_cell()),
        value,
    }
}

#[test]
fn the_order_of_the_declared_formulas_does_not_change_the_cells() {
    let declared = calculated(generated());
    let mut reversed = generated();
    reversed.formulas.reverse();
    let reversed = calculated(reversed);

    assert_eq!(cells(&reversed), cells(&declared));
    let order = |calculated: &Calculated| -> Vec<MeasureGlobalId> {
        calculated
            .measures()
            .iter()
            .map(Measure::global_id)
            .collect()
    };
    assert_eq!(order(&reversed), order(&declared));
}

#[test]
fn an_edit_gives_the_same_cells_for_every_order_of_the_declared_formulas() {
    let mut reversed = generated();
    reversed.formulas.reverse();
    let edit = |calculated: Calculated| {
        let mut edits = Edits::default();
        calculated
            .edit_check()
            .add(&quantity(Some(123.5)), &mut edits)
            .expect("the edit fits the input");
        calculated.apply(edits).expect("the edit recalculates")
    };

    assert_eq!(
        cells(&edit(calculated(reversed))),
        cells(&edit(calculated(generated())))
    );
}

#[test]
fn formulas_that_read_each_other_in_a_cycle_are_an_error() {
    let WorkspaceError::Cycle(cycle) =
        error(|unchecked| formula(unchecked, SALES).text = "'粗利' + '原価'".into())
    else {
        panic!("売上 reads 粗利, and 粗利 reads 売上")
    };
    assert!(cycle.contains(&SALES) && cycle.contains(&GROSS_PROFIT));
    assert!(!cycle.contains(&COST), "原価 reads no formula of the cycle");
    assert!(matches!(
        error(|unchecked| formula(unchecked, COST).text = "'原価' + 1".into()),
        WorkspaceError::Cycle(cycle) if cycle.contains(&COST)
    ));
}

#[test]
fn two_dimensions_or_two_measures_with_one_id_are_an_error() {
    assert_eq!(
        error(|unchecked| {
            let again = representative::dimensions(&Size::TINY, 7).remove(0);
            unchecked.dimensions.push(again);
        }),
        WorkspaceError::DuplicateDimension(DimensionGlobalId(Ulid(1)))
    );
    assert_eq!(
        error(|unchecked| {
            let result = &mut formula(unchecked, COST).result;
            *result =
                MeasureDefinition::try_new(QUANTITY, result.name().into(), result.axes().to_vec())
                    .unwrap();
        }),
        WorkspaceError::DuplicateMeasure(QUANTITY)
    );
}

#[test]
fn one_name_for_two_measures_or_for_a_measure_and_a_dimension_is_an_error() {
    assert_eq!(
        error(|unchecked| renamed(&mut formula(unchecked, COST).result, "売上")),
        WorkspaceError::DuplicateName("売上".into())
    );
    assert_eq!(
        error(|unchecked| renamed(&mut formula(unchecked, COST).result, "期間")),
        WorkspaceError::DuplicateName("期間".into())
    );
}

#[test]
fn an_input_of_dimensions_with_other_member_counts_is_an_error() {
    let larger = Size {
        products: 3,
        ..Size::TINY
    };
    assert_eq!(
        error(|unchecked| unchecked.dimensions = representative::dimensions(&larger, 7)),
        WorkspaceError::InputOfOtherDimensions(QUANTITY)
    );
}

#[test]
fn a_formula_error_names_the_result_and_the_step_that_failed() {
    let failed =
        |text: &str| match error(|unchecked| formula(unchecked, OVERHEAD).text = text.into()) {
            WorkspaceError::Formula { result, error } => {
                assert_eq!(result, OVERHEAD);
                error
            }
            other => panic!("{other:?}"),
        };

    assert!(matches!(failed("'数量' *"), FormulaError::Syntax(errors) if !errors.is_empty()));
    assert!(matches!(
        failed("'存在しない'"),
        FormulaError::Resolve(errors) if errors[0].kind == ResolveErrorKind::UnknownName
    ));
    assert!(matches!(
        failed("'数量'"),
        FormulaError::Axes(errors) if errors[0].kind == AxisErrorKind::ReducesAxesWithoutAggregation
    ));
    assert_eq!(
        error(|unchecked| {
            let result = &mut formula(unchecked, SALES).result;
            let mut axes = result.axes().to_vec();
            axes.push(DimensionGlobalId(Ulid(99)));
            *result =
                MeasureDefinition::try_new(result.global_id(), result.name().into(), axes).unwrap();
        }),
        WorkspaceError::Formula {
            result: SALES,
            error: FormulaError::Plan(EvaluateError::MissingDimension(DimensionGlobalId(Ulid(99)))),
        }
    );
}

fn renamed(result: &mut MeasureDefinition, name: &str) {
    *result = MeasureDefinition::try_new(result.global_id(), name.into(), result.axes().to_vec())
        .expect("a renamed definition keeps its valid axes");
}

fn dimension(id: u128, members: u32) -> Dimension {
    numbered_dimension(id, members, DimensionKind::Ordinary, &[])
}

#[test]
fn a_result_in_the_middle_of_a_formula_wider_than_64_bits_is_an_error() {
    let wide = |text: &str| {
        let dimensions: Vec<Dimension> = (1..=5).map(|id| dimension(id, 1 << 13)).collect();
        let references: Vec<&Dimension> = dimensions.iter().collect();
        let axes = |ids: &[u128]| ids.iter().map(|&id| DimensionGlobalId(Ulid(id))).collect();
        let definition = |id: u128, name: &str, ids: &[u128]| {
            MeasureDefinition::try_new(MeasureGlobalId(Ulid(id)), name.into(), axes(ids)).unwrap()
        };
        let input = |id, name, ids: &[u128]| {
            Measure::from_cells(definition(id, name, ids), Vec::new(), &references)
                .expect("39 bits fit in a packed coordinate")
        };
        let inputs = vec![input(11, "左", &[1, 2, 3]), input(12, "右", &[3, 4, 5])];
        Workspace::try_new(UncheckedWorkspace {
            inputs,
            formulas: vec![Formula {
                result: definition(21, "結果", &[3]),
                text: text.into(),
            }],
            dimensions,
        })
        .map(|_| ())
    };

    assert_eq!(
        wide("SUM('左' * '右', BY 'D3')"),
        Err(WorkspaceError::Formula {
            result: MeasureGlobalId(Ulid(21)),
            error: FormulaError::Plan(EvaluateError::PackingTooWide(
                (1..=5).map(|id| DimensionGlobalId(Ulid(id))).collect()
            )),
        })
    );
    assert_eq!(
        wide("SUM('左', BY 'D3') * SUM('右', BY 'D3')"),
        Ok(()),
        "every result in the middle has 13 bits"
    );
}

#[test]
fn only_a_cell_of_an_input_measure_passes_the_check() {
    let calculated = calculated(generated());
    let checks = calculated.edit_check();

    assert_eq!(checks.check(&quantity(Some(2.5))), Ok(()));
    assert_eq!(checks.check(&quantity(None)), Ok(()));
    assert!(checks.is_input(QUANTITY));
    assert!(!checks.is_input(SALES));
    let mut result = quantity(Some(2.5));
    result.measure = SALES;
    assert_eq!(checks.check(&result), Err(EditError::NotAnInput(SALES)));
    let mut short = quantity(Some(2.5));
    short.coordinate.pop();
    assert_eq!(
        checks.check(&short),
        Err(EditError::Cell(CellError::Length {
            expected: 5,
            found: 4,
        }))
    );
    let mut swapped = quantity(Some(2.5));
    let first = swapped.coordinate[0].0;
    swapped.coordinate[0].0 = PERIOD_OTHER;
    assert_eq!(
        checks.check(&swapped),
        Err(EditError::Cell(CellError::MissingAxis(first)))
    );
    let mut repeated = quantity(Some(2.5));
    repeated.coordinate[0] = repeated.coordinate[1];
    assert_eq!(
        checks.check(&repeated),
        Err(EditError::Cell(CellError::MissingAxis(first)))
    );
    let mut foreign = quantity(Some(2.5));
    let member = foreign.coordinate[1].1;
    foreign.coordinate[0].1 = member;
    assert_eq!(
        checks.check(&foreign),
        Err(EditError::Cell(CellError::NoMember {
            axis: first,
            member
        }))
    );
}

#[test]
fn an_edit_gives_the_same_cell_under_a_definition_that_declares_the_axes_in_another_order() {
    let mut reversed = generated();
    let original = generated();
    let dimensions: Vec<&Dimension> = original.dimensions.iter().collect();
    let input = &reversed.inputs[0];
    let axes = input.definition().axes().iter().rev().copied().collect();
    let definition = MeasureDefinition::try_new(QUANTITY, input.definition().name().into(), axes)
        .expect("the same axes");
    let cells_reversed = input
        .cells()
        .map(|(mut coordinate, value)| {
            coordinate.reverse();
            (coordinate, value)
        })
        .collect();
    reversed.inputs[0] = Measure::from_cells(definition, cells_reversed, &dimensions)
        .expect("the cells fit the reversed axes");
    let edited = |calculated: Calculated| {
        let mut edits = Edits::default();
        calculated
            .edit_check()
            .add(&quantity(Some(123.5)), &mut edits)
            .expect("the edit fits the input");
        calculated.apply(edits).expect("the edit recalculates")
    };

    let declared = edited(calculated(generated()));
    let reversed = edited(calculated(reversed));

    let mut cell = quantity_cell();
    cell.reverse();
    assert_eq!(reversed.inputs()[0].get(&cell), Ok(Some(123.5)));
    assert_eq!(declared.inputs()[0].get(&quantity_cell()), Ok(Some(123.5)));
    let results = |calculated: &Calculated| {
        let mut all = cells(calculated);
        all.remove(&QUANTITY);
        all
    };
    assert_eq!(results(&reversed), results(&declared));
}

#[test]
fn applied_edits_give_the_cells_of_a_workspace_that_starts_with_the_edited_inputs() {
    let start = calculated(generated());
    let mut edits = Edits::default();
    for edit in [quantity(Some(40.0)), quantity(None), quantity(Some(7.25))] {
        start
            .edit_check()
            .add(&edit, &mut edits)
            .expect("the edit fits");
    }
    let applied = start.apply(edits).expect("the edits recalculate");

    let mut edited = generated();
    let input = &mut edited.inputs[0];
    assert_eq!(
        input.global_id(),
        QUANTITY,
        "the first input is the quantity"
    );
    let mut delta = input.delta();
    delta
        .set(&quantity_cell(), 7.25)
        .expect("the coordinate fits");
    *input = input
        .apply(delta.finish())
        .expect("the delta is of the input");
    let expected = calculated(edited);

    assert_eq!(cells(&applied), cells(&expected));
    assert_ne!(cells(&applied), cells(&start));
    let global_ids = |measures: &[Measure]| -> Vec<MeasureGlobalId> {
        measures.iter().map(Measure::global_id).collect()
    };
    assert_eq!(
        global_ids(applied.inputs()),
        global_ids(&generated().inputs)
    );
    assert_eq!(applied.dimensions().len(), generated().dimensions.len());
}

#[test]
fn a_workspace_gives_back_what_it_was_made_from() {
    let workspace = Workspace::try_new(generated()).expect("the representative model is valid");
    assert_eq!(workspace.dimensions().len(), generated().dimensions.len());
    assert_eq!(workspace.inputs().len(), generated().inputs.len());
    assert!(
        workspace
            .dimensions()
            .iter()
            .any(|dimension| dimension.global_id() == PERIOD)
    );
    let formulas: Vec<Formula> = workspace
        .formulas()
        .iter()
        .map(|formula| Formula {
            result: formula.result().clone(),
            text: formula.text().to_owned(),
        })
        .collect();
    assert_eq!(formulas, generated().formulas);
}

#[test]
fn a_workspace_takes_only_inputs_of_its_definitions_and_dimensions() {
    let workspace = || Workspace::try_new(generated()).expect("the representative model is valid");
    let larger = Size {
        products: 3,
        ..Size::TINY
    };
    let mut fewer = generated().inputs;
    fewer.pop();
    for other in [fewer, representative::generate(&larger, 7).inputs] {
        let unchanged = workspace()
            .with_inputs(other)
            .expect_err("the inputs are not of the workspace");
        assert_eq!(unchanged.inputs().len(), generated().inputs.len());
    }

    let mut edited = generated().inputs;
    let mut delta = edited[0].delta();
    delta
        .set(&quantity_cell(), 7.25)
        .expect("the coordinate fits");
    edited[0] = edited[0]
        .apply(delta.finish())
        .expect("the delta is of the input");
    let cells = |inputs: &[Measure]| -> Vec<Cells> {
        inputs
            .iter()
            .map(|input| exact(&input.cells().collect::<Vec<_>>()))
            .collect()
    };
    let resumed = workspace()
        .with_inputs(edited.clone())
        .expect("the inputs are of the workspace");
    assert_eq!(cells(resumed.inputs()), cells(&edited));
    assert_ne!(cells(resumed.inputs()), cells(&generated().inputs));

    let mut reversed = edited.clone();
    reversed.reverse();
    let resumed = workspace()
        .with_inputs(reversed)
        .expect("the order of the inputs does not matter");
    assert_eq!(cells(resumed.inputs()), cells(&edited));
}

#[test]
fn an_input_of_a_dimension_of_another_kind_is_an_error() {
    let ordinary = [dimension(1, 2), dimension(2, 2)];
    let definition = MeasureDefinition::try_new(
        MeasureGlobalId(Ulid(10)),
        "入力".into(),
        ordinary.iter().map(Dimension::global_id).collect(),
    )
    .unwrap();
    let input = || {
        Measure::from_cells(
            definition.clone(),
            vec![([1, 0].map(MemberIndex).to_vec(), 1.0)],
            &ordinary.iter().collect::<Vec<_>>(),
        )
        .expect("the cell fits the dimensions")
    };
    let workspace = |dimensions: Vec<Dimension>, inputs: Vec<Measure>| {
        Workspace::try_new(UncheckedWorkspace {
            dimensions,
            inputs,
            formulas: Vec::new(),
        })
    };
    let calendar = || {
        vec![
            dimension(1, 2),
            numbered_dimension(2, 2, DimensionKind::Calendar, &[]),
        ]
    };

    assert_eq!(
        workspace(calendar(), vec![input()]).err(),
        Some(WorkspaceError::InputOfOtherDimensions(
            definition.global_id()
        ))
    );
    let made_for_calendar = Measure::from_cells(
        definition.clone(),
        Vec::new(),
        &calendar().iter().collect::<Vec<_>>(),
    )
    .expect("an empty measure fits the dimensions");
    let unchanged = workspace(calendar(), vec![made_for_calendar])
        .expect("the input is of the dimensions")
        .with_inputs(vec![input()])
        .expect_err("the input is of dimensions of another kind");
    assert_eq!(unchanged.inputs()[0].cells().count(), 0);
    let same = input();
    assert!(workspace(Vec::from(ordinary), vec![same]).is_ok());
}

#[test]
fn a_reference_attribute_outside_the_dimension_it_points_at_is_an_error() {
    let division = DimensionGlobalId(Ulid(4));
    let expected = WorkspaceError::ReferenceOutsideTarget {
        dimension: DimensionGlobalId(Ulid(5)),
        attribute: AttributeGlobalId(Ulid(103)),
    };
    let position = |unchecked: &UncheckedWorkspace| {
        unchecked
            .dimensions
            .iter()
            .position(|dimension| dimension.global_id() == division)
            .expect("the representative model has the divisions")
    };

    let fewer = error(|unchecked| {
        let position = position(unchecked);
        unchecked.dimensions[position] = dimension(4, 2);
    });
    assert_eq!(fewer, expected);
    let missing = error(|unchecked| {
        let position = position(unchecked);
        unchecked.dimensions.remove(position);
    });
    assert_eq!(missing, expected);
}

#[test]
fn a_workspace_global_id_is_one_ulid_in_any_case() {
    let upper: WorkspaceGlobalId = "01JCQ7ZK3M5V8W9X0Y1Z2A3B4C".parse().expect("a ULID");
    let lower: WorkspaceGlobalId = "01jcq7zk3m5v8w9x0y1z2a3b4c".parse().expect("a ULID");
    assert_eq!(upper, lower);
    assert_eq!(upper.to_string(), "01JCQ7ZK3M5V8W9X0Y1Z2A3B4C");
}

#[test]
fn a_workspace_global_id_above_128_bits_is_not_another_workspace() {
    for text in [
        "81JCQ7ZK3M5V8W9X0Y1Z2A3B4C",
        "ZZZZZZZZZZZZZZZZZZZZZZZZZZ",
        "demo~",
        "",
    ] {
        assert_eq!(
            text.parse::<WorkspaceGlobalId>(),
            Err(InvalidWorkspaceGlobalId),
            "{text:?}"
        );
    }
    assert_eq!(InvalidWorkspaceGlobalId.to_string(), "not a ULID");
}

#[test]
fn a_coordinate_names_a_member_by_its_global_id_in_whatever_row_the_member_is() {
    let dimension = numbered_dimension(1, 3, DimensionKind::Ordinary, &[]);
    let member = dimension
        .member(MemberIndex(1))
        .expect("three members")
        .global_id();
    let moved = Dimension::try_new(
        dimension.definition().clone(),
        dimension.batch().slice(1, 2),
        &[],
    )
    .expect("a valid dimension");
    let axes = [dimension.global_id()];
    let coordinate = [(axes[0], member)];

    assert_eq!(
        resolve(&axes, &[dimension], &coordinate),
        Ok(vec![MemberIndex(1)])
    );
    assert_eq!(
        resolve(&axes, &[moved], &coordinate),
        Ok(vec![MemberIndex(0)])
    );
}

#[test]
fn a_cell_error_names_what_the_coordinate_lacks() {
    let axis = DimensionGlobalId(Ulid(1));
    let member = dimension::MemberGlobalId(Ulid(2));
    assert_eq!(
        CellError::Length {
            expected: 5,
            found: 4
        }
        .to_string(),
        "the coordinate needs 5 axes, but it has 4"
    );
    assert_eq!(
        CellError::MissingAxis(axis).to_string(),
        format!("the coordinate has no member of axis {}", axis.0)
    );
    assert_eq!(
        CellError::NoMember { axis, member }.to_string(),
        format!("member {} is not in dimension {}", member.0, axis.0)
    );
}
