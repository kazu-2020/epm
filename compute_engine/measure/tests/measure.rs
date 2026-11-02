use std::sync::Arc;

use arrow_array::{
    ArrayRef, FixedSizeBinaryArray, Float64Array, RecordBatch, StringArray, UInt32Array,
};
use arrow_schema::{Field, Schema};
use dimension::{Dimension, DimensionDefinition, DimensionUlid, KeyKind, MemberId};
use measure::{Measure, MeasureDefinition, MeasureError, MeasureUlid, normalize};
use ulid::Ulid;

const PERIOD: DimensionUlid = DimensionUlid(Ulid(100));
const DEPARTMENT: DimensionUlid = DimensionUlid(Ulid(200));
const SALES: MeasureUlid = MeasureUlid(Ulid(300));
const COST: MeasureUlid = MeasureUlid(Ulid(400));

fn dimension(ulid: DimensionUlid, names: &[&str]) -> Dimension {
    let definition = DimensionDefinition {
        ulid,
        name: "ディメンション".into(),
        key_kind: KeyKind::Name,
        attributes: Vec::new(),
    };
    let ulids = FixedSizeBinaryArray::try_from_iter(
        (0..names.len()).map(|row| Ulid(ulid.0.0 + row as u128 + 1).to_bytes()),
    )
    .unwrap();
    let columns: Vec<ArrayRef> = vec![Arc::new(ulids), Arc::new(StringArray::from(names.to_vec()))];
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    Dimension::try_new(definition, batch, &[]).unwrap()
}

fn periods() -> Dimension {
    dimension(PERIOD, &["1月", "2月", "3月"])
}

fn departments() -> Dimension {
    dimension(DEPARTMENT, &["営業部", "開発部"])
}

fn definition(ulid: MeasureUlid, axes: Vec<DimensionUlid>) -> MeasureDefinition {
    MeasureDefinition {
        ulid,
        name: "売上".into(),
        axes,
    }
}

fn columns(rows: &[(u32, u32, f64)]) -> Vec<ArrayRef> {
    vec![
        Arc::new(UInt32Array::from_iter_values(rows.iter().map(|row| row.0))),
        Arc::new(UInt32Array::from_iter_values(rows.iter().map(|row| row.1))),
        Arc::new(Float64Array::from_iter_values(rows.iter().map(|row| row.2))),
    ]
}

fn try_sales(rows: &[(u32, u32, f64)]) -> Result<Measure, MeasureError> {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns(rows)).unwrap();
    Measure::try_new(definition, batch, &[&periods(), &departments()])
}

fn sales() -> Measure {
    try_sales(&[(0, 0, 10.0), (0, 1, 20.0), (2, 0, 30.0)]).unwrap()
}

fn coordinate(period: u32, department: u32) -> [MemberId; 2] {
    [MemberId(period), MemberId(department)]
}

fn rows(measure: &Measure) -> Vec<(u32, u32, f64)> {
    let columns = measure.base().columns();
    let periods = columns[0].as_any().downcast_ref::<UInt32Array>().unwrap();
    let departments = columns[1].as_any().downcast_ref::<UInt32Array>().unwrap();
    let values = columns[2].as_any().downcast_ref::<Float64Array>().unwrap();
    (0..values.len())
        .map(|row| {
            (
                periods.value(row),
                departments.value(row),
                values.value(row),
            )
        })
        .collect()
}

#[test]
fn cell_is_read_by_its_member_ids_and_missing_cells_are_empty() {
    let measure = sales();
    assert_eq!(measure.get(&coordinate(0, 1)).unwrap(), Some(20.0));
    assert_eq!(measure.get(&coordinate(2, 0)).unwrap(), Some(30.0));
    assert_eq!(measure.get(&coordinate(1, 0)).unwrap(), None);
    assert_eq!(
        measure.get(&[MemberId(0)]).unwrap_err(),
        MeasureError::CoordinateLength {
            expected: 2,
            found: 1
        }
    );
}

#[test]
fn measure_without_cells_is_valid() {
    let measure = try_sales(&[]).unwrap();
    assert_eq!(
        measure.definition(),
        &definition(SALES, vec![PERIOD, DEPARTMENT])
    );
    assert_eq!(measure.get(&coordinate(0, 0)).unwrap(), None);
}

#[test]
fn repeated_axis_is_rejected() {
    let definition = definition(SALES, vec![PERIOD, PERIOD]);
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns(&[])).unwrap();
    assert_eq!(
        Measure::try_new(definition, batch, &[&periods()]).unwrap_err(),
        MeasureError::DuplicateAxis(PERIOD)
    );
}

#[test]
fn axis_without_its_dimension_is_rejected() {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns(&[])).unwrap();
    assert_eq!(
        Measure::try_new(definition, batch, &[&periods()]).unwrap_err(),
        MeasureError::MissingDimension(DEPARTMENT)
    );
}

#[test]
fn base_not_matching_the_definition_is_rejected() {
    let definition = definition(SALES, vec![DEPARTMENT, PERIOD]);
    let other = self::definition(SALES, vec![PERIOD, DEPARTMENT]);
    let batch = RecordBatch::try_new(Arc::new(other.schema()), columns(&[])).unwrap();
    assert!(matches!(
        Measure::try_new(definition, batch, &[&periods(), &departments()]),
        Err(MeasureError::SchemaMismatch { .. })
    ));
}

#[test]
fn null_in_base_is_rejected() {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let fields: Vec<Field> = definition
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone().with_nullable(true))
        .collect();
    let mut columns = columns(&[(0, 0, 1.0)]);
    columns[2] = Arc::new(Float64Array::from(vec![None]));
    let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap();
    assert_eq!(
        Measure::try_new(definition, batch, &[&periods(), &departments()]).unwrap_err(),
        MeasureError::NullInColumn("value".into())
    );
}

#[test]
fn base_keeps_the_definition_schema_even_when_given_nullable_columns() {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let fields: Vec<Field> = definition
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone().with_nullable(true))
        .collect();
    let batch =
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns(&[(0, 0, 1.0)])).unwrap();
    let measure =
        Measure::try_new(definition.clone(), batch, &[&periods(), &departments()]).unwrap();
    assert_eq!(measure.base().schema().as_ref(), &definition.schema());
}

#[test]
fn member_outside_the_axis_dimension_is_rejected() {
    assert_eq!(
        try_sales(&[(0, 2, 1.0)]).unwrap_err(),
        MeasureError::MemberOutOfRange {
            axis: DEPARTMENT,
            member: MemberId(2)
        }
    );
}

#[test]
fn base_must_be_sorted_by_coordinate_without_repeats() {
    assert_eq!(
        try_sales(&[(1, 0, 1.0), (0, 1, 2.0)]).unwrap_err(),
        MeasureError::UnsortedCoordinate(1)
    );
    assert_eq!(
        try_sales(&[(0, 1, 1.0), (0, 1, 2.0)]).unwrap_err(),
        MeasureError::DuplicateCoordinate(1)
    );
}

#[test]
fn base_holds_only_normalized_values() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0] {
        assert_eq!(
            try_sales(&[(0, 0, 1.0), (0, 1, value)]).unwrap_err(),
            MeasureError::UnnormalizedValue(1)
        );
    }
}

#[test]
fn normalize_turns_non_finite_into_empty_and_negative_zero_into_zero() {
    assert_eq!(normalize(f64::NAN), None);
    assert_eq!(normalize(f64::INFINITY), None);
    assert_eq!(normalize(-0.0).map(f64::to_bits), Some(0.0f64.to_bits()));
    assert_eq!(normalize(1.5), Some(1.5));
}

#[test]
fn delta_overrides_base_and_newer_delta_wins() {
    let measure = sales();
    let mut first = measure.delta();
    first.set(&coordinate(0, 0), 11.0).unwrap();
    first.set(&coordinate(1, 1), 5.0).unwrap();
    first.delete(&coordinate(0, 1)).unwrap();
    let measure = measure.apply(first).unwrap();
    let mut second = measure.delta();
    second.set(&coordinate(1, 1), 6.0).unwrap();
    let measure = measure.apply(second).unwrap();

    assert_eq!(measure.get(&coordinate(0, 0)).unwrap(), Some(11.0));
    assert_eq!(measure.get(&coordinate(0, 1)).unwrap(), None);
    assert_eq!(measure.get(&coordinate(1, 1)).unwrap(), Some(6.0));
    assert_eq!(measure.get(&coordinate(2, 0)).unwrap(), Some(30.0));
    assert_eq!(measure.delta_len(), 4);
}

#[test]
fn later_write_in_one_delta_replaces_the_earlier_one() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(1, 0), 1.0).unwrap();
    delta.set(&coordinate(1, 0), 2.0).unwrap();
    assert_eq!(delta.len(), 1);
    let measure = measure.apply(delta).unwrap();
    assert_eq!(measure.get(&coordinate(1, 0)).unwrap(), Some(2.0));
}

#[test]
fn writing_non_finite_empties_the_cell_and_zero_is_kept() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(0, 0), f64::NAN).unwrap();
    delta.set(&coordinate(0, 1), 0.0).unwrap();
    delta.set(&coordinate(1, 0), -0.0).unwrap();
    let measure = measure.apply(delta).unwrap();
    assert_eq!(measure.get(&coordinate(0, 0)).unwrap(), None);
    assert_eq!(measure.get(&coordinate(0, 1)).unwrap(), Some(0.0));
    assert_eq!(
        measure.get(&coordinate(1, 0)).unwrap().map(f64::to_bits),
        Some(0.0f64.to_bits())
    );
}

#[test]
fn delta_rejects_coordinates_that_do_not_fit_the_measure() {
    let measure = sales();
    let mut delta = measure.delta();
    assert_eq!(
        delta.set(&[MemberId(0)], 1.0).unwrap_err(),
        MeasureError::CoordinateLength {
            expected: 2,
            found: 1
        }
    );
    assert_eq!(
        delta.delete(&coordinate(3, 0)).unwrap_err(),
        MeasureError::MemberOutOfRange {
            axis: PERIOD,
            member: MemberId(3)
        }
    );
    assert!(delta.is_empty());
}

#[test]
fn delta_made_against_dimensions_of_another_size_is_rejected() {
    let larger = sales();
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns(&[])).unwrap();
    let smaller = Measure::try_new(
        definition,
        batch,
        &[&dimension(PERIOD, &["1月", "2月"]), &departments()],
    )
    .unwrap();
    let mut delta = larger.delta();
    delta.set(&coordinate(2, 0), 1.0).unwrap();
    assert_eq!(
        smaller.apply(delta).unwrap_err(),
        MeasureError::DeltaAxesMismatch
    );
}

#[test]
fn delta_of_another_measure_is_rejected() {
    let measure = sales();
    let definition = definition(COST, vec![PERIOD, DEPARTMENT]);
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns(&[])).unwrap();
    let cost = Measure::try_new(definition, batch, &[&periods(), &departments()]).unwrap();
    assert_eq!(
        measure.apply(cost.delta()).unwrap_err(),
        MeasureError::ForeignDelta(COST)
    );
}

#[test]
fn compaction_folds_deltas_into_a_sorted_base_without_changing_reads() {
    let measure = sales();
    let mut first = measure.delta();
    first.set(&coordinate(0, 0), 11.0).unwrap();
    first.delete(&coordinate(0, 1)).unwrap();
    first.set(&coordinate(1, 1), 5.0).unwrap();
    first.set(&coordinate(2, 1), 7.0).unwrap();
    let measure = measure.apply(first).unwrap();
    let mut second = measure.delta();
    second.delete(&coordinate(2, 1)).unwrap();
    second.set(&coordinate(1, 0), 0.0).unwrap();
    let measure = measure.apply(second).unwrap();

    let measure = measure.compact();

    assert_eq!(measure.delta_len(), 0);
    assert_eq!(
        rows(&measure),
        vec![(0, 0, 11.0), (1, 0, 0.0), (1, 1, 5.0), (2, 0, 30.0)]
    );
    let rebuilt = Measure::try_new(
        definition(SALES, vec![PERIOD, DEPARTMENT]),
        measure.base().clone(),
        &[&periods(), &departments()],
    );
    assert!(rebuilt.is_ok());
}

#[test]
fn measure_without_axes_is_rejected() {
    let definition = definition(SALES, Vec::new());
    let batch = RecordBatch::try_new(
        Arc::new(definition.schema()),
        vec![Arc::new(Float64Array::from(vec![0.1])) as ArrayRef],
    )
    .unwrap();
    assert_eq!(
        Measure::try_new(definition, batch, &[]).unwrap_err(),
        MeasureError::NoAxes
    );
}

#[test]
fn applying_a_delta_leaves_the_earlier_version_unchanged() {
    let before = sales();
    let mut delta = before.delta();
    delta.set(&coordinate(0, 0), 11.0).unwrap();
    let after = before.apply(delta).unwrap();
    assert_eq!(before.get(&coordinate(0, 0)).unwrap(), Some(10.0));
    assert_eq!(after.get(&coordinate(0, 0)).unwrap(), Some(11.0));

    let compacted = after.compact();
    assert_eq!(after.delta_len(), 1);
    assert_eq!(compacted.get(&coordinate(0, 0)).unwrap(), Some(11.0));
}

#[test]
fn committer_replays_the_deltas_it_applied_during_compaction_onto_the_compacted_version() {
    let measure = sales();
    let mut first = measure.delta();
    first.set(&coordinate(0, 0), 11.0).unwrap();
    let started = measure.apply(first).unwrap();
    let compacted = started.compact();
    let mut later = started.delta();
    later.delete(&coordinate(2, 0)).unwrap();
    let later = Arc::new(later);
    let current = started.apply(later.clone()).unwrap();

    let replayed = compacted.apply(later).unwrap();

    for cell in [coordinate(0, 0), coordinate(0, 1), coordinate(2, 0)] {
        assert_eq!(replayed.get(&cell).unwrap(), current.get(&cell).unwrap());
    }
    assert_eq!(replayed.delta_len(), 1);
}

#[test]
fn delta_lists_each_coordinate_with_its_latest_change() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(1, 0), 1.0).unwrap();
    delta.delete(&coordinate(0, 1)).unwrap();
    delta.set(&coordinate(1, 0), 2.0).unwrap();
    let changes: Vec<(Vec<MemberId>, Option<f64>)> = delta
        .changes()
        .map(|(coordinate, change)| (coordinate.to_vec(), change))
        .collect();
    assert_eq!(
        changes,
        vec![
            (coordinate(1, 0).to_vec(), Some(2.0)),
            (coordinate(0, 1).to_vec(), None)
        ]
    );
}

#[test]
fn folding_deltas_leaves_one_layer_over_the_same_base() {
    let measure = sales();
    let mut first = measure.delta();
    first.set(&coordinate(0, 0), 11.0).unwrap();
    first.set(&coordinate(1, 1), 5.0).unwrap();
    let measure = measure.apply(first).unwrap();
    let mut second = measure.delta();
    second.delete(&coordinate(1, 1)).unwrap();
    second.set(&coordinate(0, 0), 12.0).unwrap();
    let measure = measure.apply(second).unwrap();

    let folded = measure.fold_deltas();

    assert_eq!(measure.delta_len(), 4);
    assert_eq!(folded.delta_len(), 2);
    assert_eq!(rows(&folded), rows(&measure));
    assert_eq!(folded.get(&coordinate(0, 0)).unwrap(), Some(12.0));
    assert_eq!(folded.get(&coordinate(1, 1)).unwrap(), None);
    assert_eq!(folded.get(&coordinate(0, 1)).unwrap(), Some(20.0));
}
