use std::sync::Arc;

use arrow_array::{
    ArrayRef, FixedSizeBinaryArray, Float64Array, RecordBatch, StringArray, UInt32Array,
    cast::AsArray, types::Float64Type,
};
use arrow_schema::{Field, Schema};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, Dimension, DimensionDefinition,
    DimensionGlobalId, DimensionKind, MemberIndex,
};
use measure::{
    CoordinateError, CoordinatePacking, Measure, MeasureDefinition, MeasureError, MeasureGlobalId,
    normalize,
};
use ulid::Ulid;

const PERIOD: DimensionGlobalId = DimensionGlobalId(Ulid(100));
const DEPARTMENT: DimensionGlobalId = DimensionGlobalId(Ulid(200));
const SALES: MeasureGlobalId = MeasureGlobalId(Ulid(300));
const COST: MeasureGlobalId = MeasureGlobalId(Ulid(400));

fn dimension(global_id: DimensionGlobalId, natural_keys: &[&str]) -> Dimension {
    let natural_key_attribute = AttributeGlobalId(global_id.0);
    let definition = DimensionDefinition::try_new(
        global_id,
        "ディメンション".into(),
        DimensionKind::Ordinary,
        natural_key_attribute,
        vec![AttributeDefinition {
            global_id: natural_key_attribute,
            name: "ID".into(),
            kind: AttributeKind::Text,
        }],
    )
    .unwrap();
    let global_ids = FixedSizeBinaryArray::try_from_iter(
        (0..natural_keys.len()).map(|row| Ulid(global_id.0.0 + row as u128 + 1).to_bytes()),
    )
    .unwrap();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(global_ids),
        Arc::new(StringArray::from(natural_keys.to_vec())),
    ];
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    Dimension::try_new(definition, batch, &[]).unwrap()
}

fn periods() -> Dimension {
    dimension(PERIOD, &["1月", "2月", "3月"])
}

fn departments() -> Dimension {
    dimension(DEPARTMENT, &["営業部", "開発部"])
}

fn definition(global_id: MeasureGlobalId, axes: Vec<DimensionGlobalId>) -> MeasureDefinition {
    MeasureDefinition {
        global_id,
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

fn coordinate(period: u32, department: u32) -> [MemberIndex; 2] {
    [MemberIndex(period), MemberIndex(department)]
}

fn rows(measure: &Measure) -> Vec<(u32, u32, f64)> {
    let base = measure.to_record_batch();
    let columns = base.columns();
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
        measure.get(&[MemberIndex(0)]).unwrap_err(),
        CoordinateError::Length {
            expected: 2,
            found: 1
        }
    );
}

#[test]
fn reading_a_member_outside_the_dimension_is_an_error_like_writing_it() {
    let measure = sales();
    let outside = CoordinateError::MemberOutOfRange {
        axis: DEPARTMENT,
        member: MemberIndex(2),
    };

    assert_eq!(measure.get(&coordinate(0, 2)).unwrap_err(), outside);
    assert_eq!(
        measure.delta().set(&coordinate(0, 2), 1.0).unwrap_err(),
        outside
    );
}

#[test]
fn coordinate_errors_tell_the_client_what_is_wrong() {
    let measure = sales();

    assert_eq!(
        measure.get(&[MemberIndex(0)]).unwrap_err().to_string(),
        "the coordinate needs 2 members, but it has 1"
    );
    assert_eq!(
        measure.get(&coordinate(0, 2)).unwrap_err().to_string(),
        format!("member 2 is not in dimension {}", DEPARTMENT.0)
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
    assert_eq!(
        measure.to_record_batch().schema().as_ref(),
        &definition.schema()
    );
}

#[test]
fn member_outside_the_axis_dimension_is_rejected() {
    assert_eq!(
        try_sales(&[(0, 2, 1.0)]).unwrap_err(),
        MeasureError::Coordinate(CoordinateError::MemberOutOfRange {
            axis: DEPARTMENT,
            member: MemberIndex(2)
        })
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

fn merged_changes(measure: &Measure) -> Vec<(Vec<MemberIndex>, Option<f64>)> {
    let (packed_coordinates, changes) = measure.merged_changes();
    packed_coordinates
        .iter()
        .map(|&packed| measure.packing().unpack(packed).map(MemberIndex).collect())
        .zip(changes)
        .collect()
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
    assert_eq!(
        merged_changes(&measure),
        [
            (coordinate(0, 0).to_vec(), Some(11.0)),
            (coordinate(0, 1).to_vec(), None),
            (coordinate(1, 1).to_vec(), Some(6.0)),
        ]
    );
}

#[test]
fn later_write_in_one_delta_replaces_the_earlier_one() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(1, 0), 1.0).unwrap();
    delta.set(&coordinate(0, 1), 3.0).unwrap();
    delta.set(&coordinate(1, 0), 2.0).unwrap();
    assert_eq!(delta.packed_coordinates().len(), 2);
    assert!(delta.packed_coordinates().is_sorted());
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
        delta.set(&[MemberIndex(0)], 1.0).unwrap_err(),
        CoordinateError::Length {
            expected: 2,
            found: 1
        }
    );
    assert_eq!(
        delta.delete(&coordinate(3, 0)).unwrap_err(),
        CoordinateError::MemberOutOfRange {
            axis: PERIOD,
            member: MemberIndex(3)
        }
    );
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
        measure.to_record_batch().clone(),
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
    assert_eq!(
        merged_changes(&measure.apply(delta).unwrap()),
        vec![
            (coordinate(0, 1).to_vec(), None),
            (coordinate(1, 0).to_vec(), Some(2.0))
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
    assert_eq!(
        folded.base_packed_coordinates(),
        measure.base_packed_coordinates()
    );
    assert_eq!(folded.base_values(), measure.base_values());
    assert_eq!(folded.get(&coordinate(0, 0)).unwrap(), Some(12.0));
    assert_eq!(folded.get(&coordinate(1, 1)).unwrap(), None);
    assert_eq!(folded.get(&coordinate(0, 1)).unwrap(), Some(20.0));
}

#[test]
fn folding_deltas_keeps_the_cells_that_only_one_delta_changes() {
    let older = [
        (coordinate(0, 0), 1.0),
        (coordinate(1, 1), 2.0),
        (coordinate(2, 1), 3.0),
    ];
    let newer = [(coordinate(1, 0), 4.0), (coordinate(2, 0), 5.0)];
    for (first, second) in [(&older[..], &newer[..]), (&newer[..], &older[..])] {
        let mut measure = sales();
        for cells in [first, second] {
            let mut delta = measure.delta();
            for (coordinate, value) in cells {
                delta.set(coordinate, *value).unwrap();
            }
            measure = measure.apply(delta).unwrap();
        }

        let changes = merged_changes(&measure.fold_deltas());

        let mut expected: Vec<(Vec<MemberIndex>, Option<f64>)> = older
            .iter()
            .chain(&newer)
            .map(|(coordinate, value)| (coordinate.to_vec(), Some(*value)))
            .collect();
        expected.sort_by_key(|(coordinate, _)| coordinate.clone());
        assert_eq!(changes, expected);
    }
}

#[test]
fn cells_given_in_any_order_build_a_sorted_measure() {
    let cells = vec![
        (coordinate(2, 0).to_vec(), 30.0),
        (coordinate(0, 1).to_vec(), 20.0),
        (coordinate(0, 0).to_vec(), 10.0),
    ];
    let measure = Measure::from_cells(
        definition(SALES, vec![PERIOD, DEPARTMENT]),
        cells,
        &[&periods(), &departments()],
    )
    .unwrap();
    assert_eq!(rows(&measure), rows(&sales()));
}

#[test]
fn cells_must_fit_the_measure() {
    let dimensions = [&periods(), &departments()];
    let sales_definition = || definition(SALES, vec![PERIOD, DEPARTMENT]);
    assert_eq!(
        Measure::from_cells(
            sales_definition(),
            vec![(vec![MemberIndex(0)], 1.0)],
            &dimensions
        )
        .unwrap_err(),
        MeasureError::Coordinate(CoordinateError::Length {
            expected: 2,
            found: 1
        })
    );
    assert_eq!(
        Measure::from_cells(
            sales_definition(),
            vec![
                (coordinate(0, 0).to_vec(), 1.0),
                (coordinate(0, 0).to_vec(), 2.0)
            ],
            &dimensions
        )
        .unwrap_err(),
        MeasureError::DuplicateCoordinate(1)
    );
}

#[test]
fn cells_read_through_the_deltas() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(1, 1), 5.0).unwrap();
    delta.delete(&coordinate(0, 1)).unwrap();
    let measure = measure.apply(delta).unwrap();
    assert_eq!(
        measure.cells().collect::<Vec<_>>(),
        [
            (coordinate(0, 0).to_vec(), 10.0),
            (coordinate(1, 1).to_vec(), 5.0),
            (coordinate(2, 0).to_vec(), 30.0),
        ]
    );
}

#[test]
fn packed_coordinates_order_cells_as_the_member_columns_do() {
    let packing = CoordinatePacking::new(&[3, 2]).unwrap();
    let packed_coordinates: Vec<u64> = [(0, 1), (1, 0), (2, 1)]
        .iter()
        .map(|&(period, department)| packing.pack([period, department]))
        .collect();
    assert!(packed_coordinates.is_sorted());
    assert_eq!(
        packing.unpack(packed_coordinates[2]).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(
        try_sales(&[(0, 1, 1.0), (2, 0, 2.0)])
            .unwrap()
            .base_packed_coordinates()
            .to_vec(),
        vec![packing.pack([0, 1]), packing.pack([2, 0])]
    );
}

#[test]
fn packings_wider_than_64_bits_are_rejected() {
    assert!(CoordinatePacking::new(&[u32::MAX, u32::MAX]).is_some());
    assert!(CoordinatePacking::new(&[u32::MAX, u32::MAX, 2]).is_none());
}

#[test]
fn a_one_member_axis_in_front_of_64_bits_packs_without_overflow() {
    let packing = CoordinatePacking::new(&[1, u32::MAX, u32::MAX]).unwrap();
    let packed = packing.pack([0, u32::MAX, 9]);
    assert_eq!(
        packing.unpack(packed).collect::<Vec<_>>(),
        vec![0, u32::MAX, 9]
    );
}

#[test]
fn one_axis_mask_holds_every_member_that_fits() {
    let packing = CoordinatePacking::new(&[5, 3]).unwrap();
    assert_eq!(packing.axis_mask(0), 0b11100);
    assert_eq!(packing.axis_mask(1), 0b00011);
    assert_eq!(packing.place(0, 4), 0b10000);
    assert_eq!(packing.place(1, 2), 0b00010);
    assert!(packing.fits(0, 7));
    assert!(!packing.fits(0, 8));
    assert!(packing.fits(1, 3));
    assert!(!packing.fits(1, 4));
}

#[test]
fn packed_coordinates_from_the_engine_are_checked_like_a_base() {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let packing = CoordinatePacking::new(&[3, 2]).unwrap();
    let dimensions = [&periods(), &departments()];
    let from_packed = |packed_coordinates: Vec<u64>| {
        let values = vec![1.0; packed_coordinates.len()];
        Measure::try_from_packed(
            definition.clone(),
            packed_coordinates.into(),
            values.into(),
            &dimensions,
        )
    };
    assert_eq!(
        from_packed(vec![packing.pack([3, 0])]).unwrap_err(),
        MeasureError::Coordinate(CoordinateError::MemberOutOfRange {
            axis: PERIOD,
            member: MemberIndex(3)
        })
    );
    assert_eq!(
        from_packed(vec![1 << 3]).unwrap_err(),
        MeasureError::BitsOutsidePacking(0)
    );
    assert_eq!(
        from_packed(vec![packing.pack([1, 0]), packing.pack([0, 1])]).unwrap_err(),
        MeasureError::UnsortedCoordinate(1)
    );
    assert_eq!(
        from_packed(vec![packing.pack([0, 1]), packing.pack([2, 0])])
            .unwrap()
            .cells()
            .collect::<Vec<_>>(),
        vec![
            (vec![MemberIndex(0), MemberIndex(1)], 1.0),
            (vec![MemberIndex(2), MemberIndex(0)], 1.0)
        ]
    );
}

#[test]
fn values_read_from_part_of_a_larger_buffer_are_copied_out() {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let mut columns = columns(&[(0, 0, 1.0), (0, 1, 2.0), (1, 0, 3.0)]);
    for column in &mut columns {
        *column = column.slice(1, 1);
    }
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    let measure = Measure::try_new(definition, batch, &[&periods(), &departments()]).unwrap();
    assert_eq!(measure.base_values().inner().capacity(), 8);
    assert_eq!(
        measure.cells().collect::<Vec<_>>(),
        vec![(vec![MemberIndex(0), MemberIndex(1)], 2.0)]
    );
}

#[test]
fn values_that_fill_their_buffer_are_kept_without_a_copy() {
    let definition = definition(SALES, vec![PERIOD, DEPARTMENT]);
    let columns = columns(&[(0, 0, 1.0), (1, 0, 3.0)]);
    let given = columns[2].as_primitive::<Float64Type>().values().clone();
    assert_eq!(given.inner().capacity(), given.inner().len());
    let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap();
    let measure = Measure::try_new(definition, batch, &[&periods(), &departments()]).unwrap();
    assert!(measure.base_values().inner().ptr_eq(given.inner()));
    assert_eq!(measure.base_values().to_vec(), vec![1.0, 3.0]);

    let batch =
        RecordBatch::try_new(Arc::new(measure.definition().schema()), crate::columns(&[])).unwrap();
    let empty = Measure::try_new(
        measure.definition().clone(),
        batch,
        &[&periods(), &departments()],
    )
    .unwrap();
    assert!(empty.base_values().is_empty());
}

#[test]
fn a_delta_from_packed_coordinates_takes_only_sorted_ones_that_fit_the_measure() {
    let measure = sales();
    let packed = |period, department| measure.packing().pack([period, department]);

    let delta = measure
        .delta_from_packed(vec![packed(0, 0), packed(1, 1)], vec![Some(-0.0), None])
        .unwrap();
    let applied = measure.apply(delta).unwrap();

    assert_eq!(
        applied.get(&coordinate(0, 0)).unwrap().map(f64::to_bits),
        Some(0.0f64.to_bits())
    );
    assert_eq!(applied.get(&coordinate(1, 1)).unwrap(), None);
    assert_eq!(
        measure
            .delta_from_packed(vec![packed(1, 1), packed(0, 0)], vec![None, None])
            .unwrap_err(),
        MeasureError::UnsortedCoordinate(1)
    );
    assert_eq!(
        measure
            .delta_from_packed(vec![packed(0, 0), packed(0, 0)], vec![None, None])
            .unwrap_err(),
        MeasureError::DuplicateCoordinate(1)
    );
    assert_eq!(
        measure
            .delta_from_packed(vec![packed(3, 0)], vec![None])
            .unwrap_err(),
        MeasureError::Coordinate(CoordinateError::MemberOutOfRange {
            axis: PERIOD,
            member: MemberIndex(3)
        })
    );
}

#[test]
fn a_measure_gives_its_axes_and_their_member_counts_in_packing_order() {
    for measure in [sales(), department_first(&[])] {
        assert_eq!(measure.axes_in_packing_order(), [PERIOD, DEPARTMENT]);
        assert_eq!(
            measure.axis_member_counts().collect::<Vec<_>>(),
            [(PERIOD, 3), (DEPARTMENT, 2)]
        );
    }
}

fn department_first(cells: &[(u32, u32, f64)]) -> Measure {
    let cells = cells
        .iter()
        .map(|&(department, period, value)| {
            (vec![MemberIndex(department), MemberIndex(period)], value)
        })
        .collect();
    Measure::from_cells(
        definition(SALES, vec![DEPARTMENT, PERIOD]),
        cells,
        &[&periods(), &departments()],
    )
    .unwrap()
}

#[test]
fn a_measure_packs_its_cells_in_packing_order_whatever_order_its_axes_are_declared_in() {
    let period_first = try_sales(&[(0, 1, 1.0), (1, 0, 3.0), (2, 0, 2.0)]).unwrap();

    let measure = department_first(&[(1, 0, 1.0), (0, 2, 2.0), (0, 1, 3.0)]);

    assert_eq!(measure.packing(), period_first.packing());
    assert_eq!(
        measure.base_packed_coordinates(),
        period_first.base_packed_coordinates()
    );
    assert_eq!(measure.base_values(), period_first.base_values());
}

#[test]
fn coordinates_are_taken_and_given_in_the_declared_order_of_the_axes() {
    let measure = department_first(&[(1, 0, 1.0), (0, 2, 2.0), (0, 1, 3.0)]);
    let at = |department, period| [MemberIndex(department), MemberIndex(period)];

    assert_eq!(measure.get(&at(0, 2)).unwrap(), Some(2.0));
    assert_eq!(measure.get(&at(1, 0)).unwrap(), Some(1.0));
    assert_eq!(measure.get(&at(1, 1)).unwrap(), None);
    assert_eq!(
        measure.get(&at(2, 0)).unwrap_err(),
        CoordinateError::MemberOutOfRange {
            axis: DEPARTMENT,
            member: MemberIndex(2)
        }
    );
    assert_eq!(
        measure.cells().collect::<Vec<_>>(),
        [
            (at(1, 0).to_vec(), 1.0),
            (at(0, 1).to_vec(), 3.0),
            (at(0, 2).to_vec(), 2.0),
        ]
    );

    let mut delta = measure.delta();
    delta.set(&at(1, 2), 9.0).unwrap();
    delta.delete(&at(0, 1)).unwrap();
    assert_eq!(
        delta.packed_coordinates(),
        [
            measure.packing().pack([1, 0]),
            measure.packing().pack([2, 1])
        ]
    );
    let edited = measure.apply(delta).unwrap();
    assert_eq!(edited.get(&at(1, 2)).unwrap(), Some(9.0));
    assert_eq!(edited.get(&at(0, 1)).unwrap(), None);
    assert_eq!(edited.get(&at(0, 2)).unwrap(), Some(2.0));
}

#[test]
fn a_record_batch_has_its_columns_in_the_declared_order_and_its_rows_in_packing_order() {
    let measure = department_first(&[(1, 0, 1.0), (0, 2, 2.0), (0, 1, 3.0)]);
    let dimensions = [&periods(), &departments()];

    let batch = measure.to_record_batch();

    assert_eq!(rows(&measure), [(1, 0, 1.0), (0, 1, 3.0), (0, 2, 2.0)]);
    let read = Measure::try_new(measure.definition().clone(), batch, &dimensions).unwrap();
    assert_eq!(
        read.base_packed_coordinates(),
        measure.base_packed_coordinates()
    );
    let try_department_first = |rows: &[(u32, u32, f64)]| {
        let definition = measure.definition().clone();
        let batch = RecordBatch::try_new(Arc::new(definition.schema()), columns(rows)).unwrap();
        Measure::try_new(definition, batch, &dimensions)
    };
    assert_eq!(
        try_department_first(&[(0, 1, 1.0), (1, 0, 2.0)]).unwrap_err(),
        MeasureError::UnsortedCoordinate(1)
    );
    assert_eq!(
        try_department_first(&[(2, 0, 1.0)]).unwrap_err(),
        MeasureError::Coordinate(CoordinateError::MemberOutOfRange {
            axis: DEPARTMENT,
            member: MemberIndex(2)
        })
    );
}

#[test]
fn base_functions_skip_the_pending_deltas_and_the_others_include_them() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(1, 1), 5.0).unwrap();
    delta.delete(&coordinate(0, 0)).unwrap();
    let measure = measure.apply(delta).unwrap();

    assert_eq!(measure.base_values().to_vec(), vec![10.0, 20.0, 30.0]);
    assert_eq!(measure.base_packed_coordinates().len(), 3);
    assert_eq!(measure.get(&coordinate(0, 0)).unwrap(), None);
    assert_eq!(measure.get(&coordinate(1, 1)).unwrap(), Some(5.0));
    assert_eq!(
        rows(&measure),
        vec![(0, 1, 20.0), (1, 1, 5.0), (2, 0, 30.0)]
    );
    assert_eq!(
        measure.cells().collect::<Vec<_>>(),
        vec![
            (coordinate(0, 1).to_vec(), 20.0),
            (coordinate(1, 1).to_vec(), 5.0),
            (coordinate(2, 0).to_vec(), 30.0),
        ]
    );
}

#[test]
fn an_empty_delta_adds_no_layer_and_a_delta_that_changes_nothing_keeps_the_cells() {
    let measure = sales();

    let unchanged = measure.apply(measure.delta()).unwrap();
    assert_eq!(unchanged.delta_len(), 0);

    let mut delta = measure.delta();
    delta.delete(&coordinate(1, 0)).unwrap();
    delta.set(&coordinate(0, 0), 10.0).unwrap();
    let applied = measure.apply(delta).unwrap();
    assert_eq!(rows(&applied), rows(&measure));
    assert_eq!(applied.get(&coordinate(1, 0)).unwrap(), None);
}

#[test]
fn a_shared_unsorted_delta_gives_the_same_cells_to_every_measure_it_is_applied_to() {
    let measure = sales();
    let mut delta = measure.delta();
    delta.set(&coordinate(2, 1), 1.0).unwrap();
    delta.set(&coordinate(0, 1), 2.0).unwrap();
    delta.set(&coordinate(2, 1), 3.0).unwrap();
    let shared = Arc::new(delta);

    let first = measure.apply(Arc::clone(&shared)).unwrap();
    let second = measure.apply(Arc::clone(&shared)).unwrap();

    let expected = vec![(0, 0, 10.0), (0, 1, 2.0), (2, 0, 30.0), (2, 1, 3.0)];
    assert_eq!(rows(&first), expected);
    assert_eq!(rows(&second), expected);
    assert_eq!(first.delta_len(), 2);
}

#[test]
fn two_axes_that_fill_64_bits_pack_and_unpack_every_member() {
    let packing = CoordinatePacking::new(&[u32::MAX, u32::MAX]).unwrap();
    let largest = u32::MAX - 1;

    let packed = packing.pack([largest, largest]);

    assert_eq!(packed, (u64::from(largest) << 32) | u64::from(largest));
    assert_eq!(
        packing.unpack(packed).collect::<Vec<_>>(),
        vec![largest, largest]
    );
    assert!(packing.pack([0, 1]) < packing.pack([1, 0]));
}
