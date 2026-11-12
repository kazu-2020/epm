use std::sync::Arc;

use arrow_array::{
    ArrayRef, FixedSizeBinaryArray, LargeStringArray, RecordBatch, StringArray, UInt32Array,
};
use arrow_schema::{DataType, Field, Schema};
use dimension::{
    AttributeDefinition, AttributeGlobalId, AttributeKind, DefinitionError, Dimension,
    DimensionDefinition, DimensionError, DimensionGlobalId, DimensionKind, MemberGlobalId,
    MemberIndex,
};
use ulid::Ulid;

const DEPARTMENT: DimensionGlobalId = DimensionGlobalId(Ulid(100));
const DIVISION: DimensionGlobalId = DimensionGlobalId(Ulid(200));
const DIVISION_ATTRIBUTE: AttributeGlobalId = AttributeGlobalId(Ulid(300));
const NATURAL_KEY: AttributeGlobalId = AttributeGlobalId(Ulid(400));
const NAME_ATTRIBUTE: AttributeGlobalId = AttributeGlobalId(Ulid(500));

fn natural_key_attribute() -> AttributeDefinition {
    AttributeDefinition {
        ulid: NATURAL_KEY,
        name: "ID".into(),
        kind: AttributeKind::Text,
    }
}

fn department(
    natural_key: AttributeGlobalId,
    attributes: Vec<AttributeDefinition>,
) -> Result<DimensionDefinition, DefinitionError> {
    DimensionDefinition::try_new(
        DEPARTMENT,
        "部門".into(),
        DimensionKind::Ordinary,
        natural_key,
        attributes,
    )
}

fn try_definition(
    attributes: Vec<AttributeDefinition>,
) -> Result<DimensionDefinition, DefinitionError> {
    department(
        NATURAL_KEY,
        [vec![natural_key_attribute()], attributes].concat(),
    )
}

fn definition(attributes: Vec<AttributeDefinition>) -> DimensionDefinition {
    try_definition(attributes).unwrap()
}

fn division_attribute(target: DimensionGlobalId) -> AttributeDefinition {
    AttributeDefinition {
        ulid: DIVISION_ATTRIBUTE,
        name: "事業部".into(),
        kind: AttributeKind::Reference(target),
    }
}

fn name_attribute() -> AttributeDefinition {
    AttributeDefinition {
        ulid: NAME_ATTRIBUTE,
        name: "名前".into(),
        kind: AttributeKind::Text,
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
    let definition = definition(vec![division_attribute(DIVISION)]);
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

fn definition_without_attributes(
    ulid: DimensionGlobalId,
    kind: DimensionKind,
) -> DimensionDefinition {
    DimensionDefinition::try_new(
        ulid,
        "事業部".into(),
        kind,
        NATURAL_KEY,
        vec![natural_key_attribute()],
    )
    .unwrap()
}

fn divisions() -> Dimension {
    let definition = definition_without_attributes(DIVISION, DimensionKind::Ordinary);
    let columns = vec![
        ulids(&[Some(11), Some(12)]),
        strings(&[Some("第一事業部"), Some("第二事業部")]),
    ];
    Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap()
}

#[test]
fn member_is_found_by_member_index_ulid_and_natural_key() {
    let dimension = departments();
    let sales = dimension.member_by_natural_key("営業部").unwrap();
    assert_eq!(sales.id(), MemberIndex(0));
    assert_eq!(sales.ulid(), MemberGlobalId(Ulid(1)));
    assert_eq!(
        dimension
            .member_by_ulid(MemberGlobalId(Ulid(2)))
            .unwrap()
            .natural_key(),
        "開発部"
    );
    assert_eq!(
        dimension.member(MemberIndex(2)).unwrap().natural_key(),
        "総務部"
    );
    assert!(dimension.member(MemberIndex(3)).is_none());
    assert!(dimension.member_by_natural_key("経理部").is_none());
    assert!(dimension.member_by_ulid(MemberGlobalId(Ulid(9))).is_none());
    assert_eq!(dimension.name(), "部門");
    assert_eq!(dimension.len(), 3);
    assert!(!dimension.is_empty());
}

#[test]
fn other_text_attributes_may_repeat_be_empty_or_be_null() {
    let definition = definition(vec![name_attribute()]);
    let columns = vec![
        ulids(&[Some(1), Some(2), Some(3), Some(4)]),
        strings(&[Some("4110"), Some("5110"), Some("6110"), Some("7110")]),
        strings(&[Some("その他"), Some("その他"), Some(""), None]),
    ];
    let dimension =
        Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap();
    assert_eq!(dimension.definition(), &definition);
    let member = dimension.member_by_natural_key("5110").unwrap();
    assert_eq!(member.id(), MemberIndex(1));
    let name = dimension.text_attribute("名前").unwrap();
    assert_eq!(name.value(member.id()), Some("その他"));
    assert_eq!(name.value(MemberIndex(2)), Some(""));
    assert_eq!(name.value(MemberIndex(3)), None);
    assert_eq!(name.value(MemberIndex(4)), None);
    assert_eq!(dimension.natural_key_attribute().name(), "ID");
    assert_eq!(dimension.natural_key_attribute().ulid(), NATURAL_KEY);
    let schema = definition.schema();
    assert!(!schema.field(1).is_nullable());
    assert!(schema.field(2).is_nullable());
}

#[test]
fn definition_keeps_its_name_and_attributes_in_order() {
    let definition = definition(vec![name_attribute()]);
    assert_eq!(definition.name(), "部門");
    assert_eq!(
        definition.attributes(),
        [natural_key_attribute(), name_attribute()]
    );
}

#[test]
fn natural_key_may_come_after_other_attributes() {
    let definition =
        department(NATURAL_KEY, vec![name_attribute(), natural_key_attribute()]).unwrap();
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("その他")]),
        strings(&[Some("4110")]),
    ];
    let dimension =
        Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap();
    assert_eq!(
        dimension.member_by_natural_key("4110").unwrap().id(),
        MemberIndex(0)
    );
    assert_eq!(dimension.natural_key_attribute().name(), "ID");
}

#[test]
fn natural_key_that_is_missing_or_not_a_text_attribute_is_rejected() {
    let natural_key_definition = |natural_key| {
        department(
            natural_key,
            vec![natural_key_attribute(), division_attribute(DIVISION)],
        )
    };
    assert_eq!(
        natural_key_definition(DIVISION_ATTRIBUTE).unwrap_err(),
        DefinitionError::NaturalKeyIsNotATextAttribute(DIVISION_ATTRIBUTE)
    );
    assert_eq!(
        natural_key_definition(NAME_ATTRIBUTE).unwrap_err(),
        DefinitionError::MissingNaturalKey(NAME_ATTRIBUTE)
    );
}

#[test]
fn attribute_value_is_a_member_of_the_target_dimension_or_none() {
    let dimension = departments();
    let attribute = dimension.reference_attribute("事業部").unwrap();
    assert_eq!(attribute.target(), DIVISION);
    assert_eq!(attribute.value(MemberIndex(1)), Some(MemberIndex(1)));
    assert_eq!(attribute.value(MemberIndex(2)), None);
    assert_eq!(attribute.value(MemberIndex(3)), None);
    assert!(dimension.reference_attribute("地域").is_none());
    assert!(dimension.reference_attribute("ID").is_none());
}

#[test]
fn attribute_without_its_target_dimension_is_rejected() {
    let definition = definition(vec![division_attribute(DIVISION)]);
    let columns = vec![
        ulids(&[Some(1)]),
        strings(&[Some("営業部")]),
        Arc::new(UInt32Array::from(vec![Some(0)])) as ArrayRef,
    ];
    let regions =
        definition_without_attributes(DimensionGlobalId(Ulid(999)), DimensionKind::Ordinary);
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
    let definition = definition(vec![division_attribute(DIVISION)]);
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
            member: MemberIndex(0),
            target: MemberIndex(2),
        }
    );
}

#[test]
fn attribute_pointing_at_its_own_dimension_is_rejected() {
    assert_eq!(
        try_definition(vec![division_attribute(DEPARTMENT)]).unwrap_err(),
        DefinitionError::AttributeTargetsItself("事業部".into())
    );
}

#[test]
fn repeated_attribute_name_is_rejected() {
    let mut second = division_attribute(DIVISION);
    second.ulid = AttributeGlobalId(Ulid(301));
    assert_eq!(
        try_definition(vec![division_attribute(DIVISION), second]).unwrap_err(),
        DefinitionError::DuplicateAttributeName("事業部".into())
    );
}

#[test]
fn repeated_attribute_ulid_is_rejected() {
    let mut second = division_attribute(DIVISION);
    second.name = "本部".into();
    assert_eq!(
        try_definition(vec![division_attribute(DIVISION), second]).unwrap_err(),
        DefinitionError::DuplicateAttributeUlid(DIVISION_ATTRIBUTE)
    );
}

#[test]
fn batch_not_matching_the_definition_is_rejected() {
    let with_attribute = definition(vec![division_attribute(DIVISION)]);
    let without_attribute = definition(vec![]);
    let columns = vec![ulids(&[Some(1)]), strings(&[Some("営業部")])];
    let err =
        Dimension::try_new(with_attribute, batch(&without_attribute, columns), &[]).unwrap_err();
    assert!(matches!(err, DimensionError::SchemaMismatch { .. }));
}

#[test]
fn null_ulid_is_rejected() {
    let definition = definition(vec![]);
    let null = nullable_batch(
        &definition,
        vec![ulids(&[None]), strings(&[Some("営業部")])],
    );
    assert_eq!(
        Dimension::try_new(definition, null, &[]).unwrap_err(),
        DimensionError::NullUlid(MemberIndex(0))
    );
}

#[test]
fn repeated_ulid_is_rejected() {
    let definition = definition(vec![]);
    let repeated = batch(
        &definition,
        vec![
            ulids(&[Some(1), Some(1)]),
            strings(&[Some("営業部"), Some("開発部")]),
        ],
    );
    assert_eq!(
        Dimension::try_new(definition, repeated, &[]).unwrap_err(),
        DimensionError::DuplicateUlid(MemberGlobalId(Ulid(1)))
    );
}

#[test]
fn null_natural_key_is_rejected() {
    let definition = definition(vec![]);
    let null = nullable_batch(&definition, vec![ulids(&[Some(1)]), strings(&[None])]);
    assert_eq!(
        Dimension::try_new(definition, null, &[]).unwrap_err(),
        DimensionError::NullNaturalKey(MemberIndex(0))
    );
}

#[test]
fn repeated_natural_key_is_rejected() {
    let definition = definition(vec![]);
    let repeated = batch(
        &definition,
        vec![
            ulids(&[Some(1), Some(2)]),
            strings(&[Some("営業部"), Some("営業部")]),
        ],
    );
    assert_eq!(
        Dimension::try_new(definition, repeated, &[]).unwrap_err(),
        DimensionError::DuplicateNaturalKey("営業部".into())
    );
}

#[test]
fn empty_natural_key_is_rejected() {
    let definition = definition(vec![]);
    let empty = batch(&definition, vec![ulids(&[Some(1)]), strings(&[Some("")])]);
    assert_eq!(
        Dimension::try_new(definition, empty, &[]).unwrap_err(),
        DimensionError::EmptyNaturalKey(MemberIndex(0))
    );
}

#[test]
fn sliced_batch_is_read_from_its_offset() {
    let definition = definition(vec![division_attribute(DIVISION)]);
    let columns = vec![
        ulids(&[Some(1), Some(1), Some(2), Some(3)]),
        strings(&[
            Some("営業部"),
            Some("営業部"),
            Some("開発部"),
            Some("総務部"),
        ]),
        Arc::new(UInt32Array::from(vec![Some(9), Some(0), Some(1), None])),
    ];
    let sliced = batch(&definition, columns).slice(1, 3);

    let dimension = Dimension::try_new(definition, sliced, &[&divisions()]).unwrap();

    assert_eq!(dimension.len(), 3);
    let sales = dimension.member_by_ulid(MemberGlobalId(Ulid(1))).unwrap();
    assert_eq!(sales.id(), MemberIndex(0));
    assert_eq!(sales.natural_key(), "営業部");
    assert_eq!(
        dimension.member_by_natural_key("総務部").unwrap().id(),
        MemberIndex(2)
    );
    let division = dimension.reference_attribute("事業部").unwrap();
    assert_eq!(division.value(MemberIndex(0)), Some(MemberIndex(0)));
    assert_eq!(division.value(MemberIndex(2)), None);
    assert_eq!(
        dimension.natural_key_attribute().value(MemberIndex(1)),
        Some("開発部")
    );
}

#[test]
fn debug_output_names_the_member_not_the_whole_dimension() {
    let dimension = departments();
    let printed = format!("{:?}", dimension.member(MemberIndex(0)).unwrap());
    assert!(printed.contains("MemberIndex(0)"));
    assert!(!printed.contains("開発部"));
}

#[test]
fn renaming_an_attribute_keeps_its_column_name() {
    let before = definition(vec![division_attribute(DIVISION)]);
    let after = department(
        NATURAL_KEY,
        vec![
            AttributeDefinition {
                name: "コード".into(),
                ..natural_key_attribute()
            },
            AttributeDefinition {
                name: "本部".into(),
                ..division_attribute(DIVISION)
            },
        ],
    )
    .unwrap();
    assert_eq!(before.schema(), after.schema());
}

#[test]
fn dimension_without_members_is_empty() {
    let definition = definition(vec![]);
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
    let definition = definition(vec![]);
    let fields = vec![
        Field::new("ulid", DataType::FixedSizeBinary(16), false),
        Field::new(NATURAL_KEY.0.to_string(), DataType::LargeUtf8, false),
    ];
    let columns: Vec<ArrayRef> = vec![
        ulids(&[Some(1)]),
        Arc::new(LargeStringArray::from(vec!["営業部"])),
    ];
    let wrong_type = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap();
    let err = Dimension::try_new(definition, wrong_type, &[]).unwrap_err();
    assert!(matches!(err, DimensionError::SchemaMismatch { .. }));
}

#[test]
fn key_order_puts_the_calendar_then_the_version_then_the_rest_by_ulid() {
    let dimension = |ulid: u128, kind| {
        let definition = definition_without_attributes(DimensionGlobalId(Ulid(ulid)), kind);
        let columns = vec![ulids(&[Some(ulid)]), strings(&[Some("メンバー")])];
        Dimension::try_new(definition.clone(), batch(&definition, columns), &[]).unwrap()
    };
    let mut dimensions = [
        dimension(1, DimensionKind::Ordinary),
        dimension(2, DimensionKind::Version),
        dimension(3, DimensionKind::Calendar),
        dimension(4, DimensionKind::Ordinary),
    ];

    dimensions.sort_by_key(Dimension::key_order);

    let ulids: Vec<u128> = dimensions
        .iter()
        .map(|dimension| dimension.ulid().0.0)
        .collect();
    assert_eq!(ulids, [3, 2, 1, 4]);
}
