use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, FixedSizeBinaryArray, LargeStringArray, RecordBatch, StringArray, UInt32Array,
};
use arrow_schema::{DataType, Field, Schema};
use dimension::{
    AttributeDefinition, AttributeUlid, Dimension, DimensionDefinition, DimensionError,
    DimensionUlid, KeyKind, MemberId, MemberUlid,
};
use ulid::Ulid;

const DEPARTMENT: DimensionUlid = DimensionUlid(Ulid(100));
const DIVISION: DimensionUlid = DimensionUlid(Ulid(200));
const DIVISION_ATTRIBUTE: AttributeUlid = AttributeUlid(Ulid(300));

fn definition(key_kind: KeyKind, attributes: Vec<AttributeDefinition>) -> DimensionDefinition {
    DimensionDefinition {
        ulid: DEPARTMENT,
        name: "部門".into(),
        key_kind,
        attributes,
    }
}

fn division_attribute(target: DimensionUlid) -> AttributeDefinition {
    AttributeDefinition {
        ulid: DIVISION_ATTRIBUTE,
        name: "事業部".into(),
        target,
    }
}

fn ulids(values: &[Option<u128>]) -> ArrayRef {
    Arc::new(
        FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            values
                .iter()
                .map(|value| value.map(|number| Ulid(number).to_bytes())),
            16,
        )
        .unwrap(),
    )
}

fn strings(values: &[Option<&str>]) -> ArrayRef {
    Arc::new(StringArray::from(values.to_vec()))
}

fn batch(definition: &DimensionDefinition, columns: Vec<ArrayRef>) -> RecordBatch {
    RecordBatch::try_new(Arc::new(definition.schema()), columns).unwrap()
}

fn nullable_batch(definition: &DimensionDefinition, columns: Vec<ArrayRef>) -> RecordBatch {
    let fields: Vec<Field> = definition
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone().with_nullable(true))
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

fn departments() -> Dimension {
    let definition = definition(KeyKind::Name, vec![division_attribute(DIVISION)]);
    let columns = vec![
        ulids(&[Some(1), Some(2), Some(3)]),
        strings(&[Some("営業部"), Some("開発部"), Some("総務部")]),
        Arc::new(UInt32Array::from(vec![Some(0), Some(1), None])),
    ];
    Dimension::try_new(
        definition.clone(),
        batch(&definition, columns),
        &[&divisions()],
    )
    .unwrap()
}

fn definition_without_attributes(ulid: DimensionUlid) -> DimensionDefinition {
    DimensionDefinition {
        ulid,
        name: "事業部".into(),
        key_kind: KeyKind::Name,
        attributes: vec![],
    }
}

fn divisions() -> Dimension {
    let definition = definition_without_attributes(DIVISION);
    let columns = vec![
        ulids(&[Some(11), Some(12)]),
        strings(&[Some("第一事業部"), Some("第二事業部")]),
    ];
    Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap()
}

#[test]
fn member_is_found_by_member_id_ulid_and_key() {
    let dimension = departments();
    let sales = dimension.member_by_key("営業部").unwrap();
    assert_eq!(sales.id(), MemberId(0));
    assert_eq!(sales.ulid(), MemberUlid(Ulid(1)));
    assert_eq!(
        dimension
            .member_by_ulid(MemberUlid(Ulid(2)))
            .unwrap()
            .name(),
        "開発部"
    );
    assert_eq!(dimension.member(MemberId(2)).unwrap().key(), "総務部");
    assert!(dimension.member(MemberId(3)).is_none());
    assert!(dimension.member_by_key("経理部").is_none());
    assert!(dimension.member_by_ulid(MemberUlid(Ulid(9))).is_none());
    assert_eq!(dimension.name(), "部門");
    assert_eq!(dimension.len(), 3);
    assert!(!dimension.is_empty());
}

#[test]
fn code_key_lets_names_repeat() {
    let definition = definition(KeyKind::Code, vec![]);
    let columns = vec![
        ulids(&[Some(1), Some(2)]),
        strings(&[Some("その他"), Some("その他")]),
        strings(&[Some("4110"), Some("5110")]),
    ];
    let dimension =
        Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap();
    let member = dimension.member_by_key("5110").unwrap();
    assert_eq!(member.id(), MemberId(1));
    assert_eq!(member.name(), "その他");
}

#[test]
fn attribute_value_is_a_member_of_the_target_dimension_or_none() {
    let dimension = departments();
    let attribute = dimension.attribute("事業部").unwrap();
    assert_eq!(attribute.target(), DIVISION);
    assert_eq!(attribute.value(MemberId(1)), Some(MemberId(1)));
    assert_eq!(attribute.value(MemberId(2)), None);
    assert_eq!(attribute.value(MemberId(3)), None);
    assert!(dimension.attribute("地域").is_none());
}

#[test]
fn attribute_without_its_target_dimension_is_rejected() {
    let definition = definition(KeyKind::Name, vec![division_attribute(DIVISION)]);
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("営業部")]),
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
    ];
    let regions = definition_without_attributes(DimensionUlid(Ulid(999)));
    let columns_of_regions = vec![ulids(&[Some(21)]), strings(&[Some("関東")])];
    let other =
        Dimension::try_new(regions.clone(), batch(&regions, columns_of_regions), &[]).unwrap();
    let err =
        Dimension::try_new(definition.clone(), batch(&definition, columns), &[&other]).unwrap_err();
    assert_eq!(
        err,
        DimensionError::MissingTarget {
            attribute: "事業部".into(),
            target: DIVISION,
        }
    );
}

#[test]
fn attribute_value_outside_the_target_dimension_is_rejected() {
    let definition = definition(KeyKind::Name, vec![division_attribute(DIVISION)]);
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("営業部")]),
        Arc::new(UInt32Array::from(vec![Some(2)])) as ArrayRef,
    ];
    let err = Dimension::try_new(
        definition.clone(),
        batch(&definition, columns),
        &[&divisions()],
    )
    .unwrap_err();
    assert_eq!(
        err,
        DimensionError::DanglingTarget {
            attribute: "事業部".into(),
            member: MemberId(0),
            target: MemberId(2),
        }
    );
}

#[test]
fn attribute_pointing_at_its_own_dimension_is_rejected() {
    let definition = definition(KeyKind::Name, vec![division_attribute(DEPARTMENT)]);
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("営業部")]),
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
    ];
    let err = Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap_err();
    assert_eq!(err, DimensionError::AttributeTargetsItself("事業部".into()));
}

#[test]
fn repeated_attribute_name_is_rejected() {
    let mut second = division_attribute(DIVISION);
    second.ulid = AttributeUlid(Ulid(301));
    let definition = definition(KeyKind::Name, vec![division_attribute(DIVISION), second]);
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("営業部")]),
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
    ];
    let err = Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap_err();
    assert_eq!(err, DimensionError::DuplicateAttributeName("事業部".into()));
}

#[test]
fn repeated_attribute_ulid_is_rejected() {
    let mut second = division_attribute(DIVISION);
    second.name = "本部".into();
    let definition = definition(KeyKind::Name, vec![division_attribute(DIVISION), second]);
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("営業部")]),
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
    ];
    let err = Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap_err();
    assert_eq!(
        err,
        DimensionError::DuplicateAttributeUlid(DIVISION_ATTRIBUTE)
    );
}

#[test]
fn batch_not_matching_the_definition_is_rejected() {
    let with_attribute = definition(KeyKind::Name, vec![division_attribute(DIVISION)]);
    let without_attribute = definition(KeyKind::Name, vec![]);
    let columns = vec![ulids(&[Some(1)]), strings(&[Some("営業部")])];
    let err =
        Dimension::try_new(with_attribute, batch(&without_attribute, columns), &[]).unwrap_err();
    assert!(matches!(err, DimensionError::SchemaMismatch { .. }));
}

#[test]
fn null_ulid_is_rejected() {
    let definition = definition(KeyKind::Name, vec![]);
    let null = nullable_batch(
        &definition,
        vec![ulids(&[None]), strings(&[Some("営業部")])],
    );
    assert_eq!(
        Dimension::try_new(definition, null, &[]).unwrap_err(),
        DimensionError::NullUlid(MemberId(0))
    );
}

#[test]
fn repeated_ulid_is_rejected() {
    let definition = definition(KeyKind::Name, vec![]);
    let repeated = batch(
        &definition,
        vec![
            ulids(&[Some(1), Some(1)]),
            strings(&[Some("営業部"), Some("開発部")]),
        ],
    );
    assert_eq!(
        Dimension::try_new(definition, repeated, &[]).unwrap_err(),
        DimensionError::DuplicateUlid(MemberUlid(Ulid(1)))
    );
}

#[test]
fn null_name_is_rejected() {
    let definition = definition(KeyKind::Name, vec![]);
    let null = nullable_batch(&definition, vec![ulids(&[Some(1)]), strings(&[None])]);
    assert_eq!(
        Dimension::try_new(definition, null, &[]).unwrap_err(),
        DimensionError::NullName(MemberId(0))
    );
}

#[test]
fn repeated_key_is_rejected() {
    let definition = definition(KeyKind::Name, vec![]);
    let repeated = batch(
        &definition,
        vec![
            ulids(&[Some(1), Some(2)]),
            strings(&[Some("営業部"), Some("営業部")]),
        ],
    );
    assert_eq!(
        Dimension::try_new(definition, repeated, &[]).unwrap_err(),
        DimensionError::DuplicateKey("営業部".into())
    );
}

#[test]
fn values_array_is_shared_for_vectorized_by() {
    let dimension = departments();
    let values = dimension.attribute("事業部").unwrap().values();
    assert_eq!(values.len(), 3);
    assert!(values.is_null(2));
}

#[test]
fn null_code_is_rejected() {
    let definition = definition(KeyKind::Code, vec![]);
    let null = nullable_batch(
        &definition,
        vec![
            ulids(&[Some(1)]),
            strings(&[Some("その他")]),
            strings(&[None]),
        ],
    );
    assert_eq!(
        Dimension::try_new(definition, null, &[]).unwrap_err(),
        DimensionError::NullKey(MemberId(0))
    );
}

#[test]
fn debug_output_names_the_member_not_the_whole_dimension() {
    let dimension = departments();
    let printed = format!("{:?}", dimension.member(MemberId(0)).unwrap());
    assert!(printed.contains("MemberId(0)"));
    assert!(!printed.contains("開発部"));
}

#[test]
fn renaming_an_attribute_keeps_its_column_name() {
    let before = definition(KeyKind::Name, vec![division_attribute(DIVISION)]);
    let mut after = before.clone();
    after.attributes[0].name = "本部".into();
    assert_eq!(before.schema(), after.schema());
}

#[test]
fn dimension_without_members_is_empty() {
    let definition = definition(KeyKind::Name, vec![]);
    let dimension = Dimension::try_new(
        definition.clone(),
        batch(&definition, vec![ulids(&[]), strings(&[])]),
        &[],
    )
    .unwrap();
    assert!(dimension.is_empty());
}

#[test]
fn column_with_the_right_name_but_wrong_type_is_rejected() {
    let definition = definition(KeyKind::Name, vec![]);
    let fields = vec![
        Field::new("ulid", DataType::FixedSizeBinary(16), false),
        Field::new("name", DataType::LargeUtf8, false),
    ];
    let columns: Vec<ArrayRef> = vec![
        ulids(&[Some(1)]),
        Arc::new(LargeStringArray::from(vec!["営業部"])),
    ];
    let wrong_type = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap();
    let err = Dimension::try_new(definition, wrong_type, &[]).unwrap_err();
    assert!(matches!(err, DimensionError::SchemaMismatch { .. }));
}
