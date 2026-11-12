use std::collections::HashSet;
use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::UInt32Type;
use arrow_array::{Array, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use ulid::Ulid;

const ULID_COLUMN: &str = "ulid";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DimensionGlobalId(pub Ulid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberGlobalId(pub Ulid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttributeGlobalId(pub Ulid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberIndex(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DimensionKind {
    Calendar,
    Version,
    Ordinary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeDefinition {
    pub ulid: AttributeGlobalId,
    pub name: String,
    pub kind: AttributeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeKind {
    Text,
    Reference(DimensionGlobalId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DimensionDefinition {
    pub ulid: DimensionGlobalId,
    pub name: String,
    pub kind: DimensionKind,
    pub natural_key_attribute: AttributeGlobalId,
    pub attributes: Vec<AttributeDefinition>,
}

impl DimensionDefinition {
    pub fn schema(&self) -> Schema {
        let mut fields = vec![Field::new(
            ULID_COLUMN,
            DataType::FixedSizeBinary(16),
            false,
        )];
        for attribute in &self.attributes {
            let data_type = match attribute.kind {
                AttributeKind::Text => DataType::Utf8,
                AttributeKind::Reference(_) => DataType::UInt32,
            };
            fields.push(Field::new(
                attribute.ulid.0.to_string(),
                data_type,
                attribute.ulid != self.natural_key_attribute,
            ));
        }
        Schema::new(fields)
    }
}

#[derive(Debug)]
pub struct Dimension {
    definition: DimensionDefinition,
    members: RecordBatch,
    ulids: FixedSizeBinaryArray,
    natural_key_position: usize,
    natural_keys: StringArray,
    attribute_values: Vec<AttributeValues>,
    hasher: RandomState,
    // Not HashMap<Ulid, MemberIndex>: the ULID and Natural Key bytes already live in the Arrow arrays.
    by_ulid: HashTable<MemberIndex>,
    by_natural_key: HashTable<MemberIndex>,
}

#[derive(Debug)]
enum AttributeValues {
    Text(StringArray),
    Reference(UInt32Array),
}

#[derive(Debug, PartialEq)]
pub enum DimensionError {
    TooManyMembers(usize),
    DuplicateAttributeName(String),
    DuplicateAttributeUlid(AttributeGlobalId),
    AttributeTargetsItself(String),
    MissingNaturalKey(AttributeGlobalId),
    NaturalKeyIsNotATextAttribute(AttributeGlobalId),
    SchemaMismatch {
        expected: SchemaRef,
        found: SchemaRef,
    },
    NullUlid(MemberIndex),
    DuplicateUlid(MemberGlobalId),
    NullNaturalKey(MemberIndex),
    DuplicateNaturalKey(String),
    MissingTarget {
        attribute: String,
        target: DimensionGlobalId,
    },
    DanglingTarget {
        attribute: String,
        member: MemberIndex,
        target: MemberIndex,
    },
}

impl fmt::Display for DimensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for DimensionError {}

impl Dimension {
    pub fn try_new(
        definition: DimensionDefinition,
        members: RecordBatch,
        targets: &[&Dimension],
    ) -> Result<Self, DimensionError> {
        u32::try_from(members.num_rows())
            .map_err(|_| DimensionError::TooManyMembers(members.num_rows()))?;
        let mut attribute_names = HashSet::new();
        let mut attribute_ulids = HashSet::new();
        for attribute in &definition.attributes {
            if !attribute_names.insert(attribute.name.as_str()) {
                return Err(DimensionError::DuplicateAttributeName(
                    attribute.name.clone(),
                ));
            }
            if !attribute_ulids.insert(attribute.ulid) {
                return Err(DimensionError::DuplicateAttributeUlid(attribute.ulid));
            }
            if attribute.kind == AttributeKind::Reference(definition.ulid) {
                return Err(DimensionError::AttributeTargetsItself(
                    attribute.name.clone(),
                ));
            }
        }
        let Some(natural_key_position) = definition
            .attributes
            .iter()
            .position(|attribute| attribute.ulid == definition.natural_key_attribute)
        else {
            return Err(DimensionError::MissingNaturalKey(
                definition.natural_key_attribute,
            ));
        };
        if definition.attributes[natural_key_position].kind != AttributeKind::Text {
            return Err(DimensionError::NaturalKeyIsNotATextAttribute(
                definition.natural_key_attribute,
            ));
        }
        let expected = definition.schema();
        let found = members.schema();
        let same_shape =
            expected.fields().len() == found.fields().len()
                && expected.fields().iter().zip(found.fields()).all(
                    |(expected_field, found_field)| {
                        expected_field.name() == found_field.name()
                            && expected_field.data_type() == found_field.data_type()
                    },
                );
        if !same_shape {
            return Err(DimensionError::SchemaMismatch {
                expected: Arc::new(expected),
                found,
            });
        }

        let columns = members.columns();
        let ulids = columns[0].as_fixed_size_binary().clone();
        let attribute_values: Vec<AttributeValues> = definition
            .attributes
            .iter()
            .zip(&columns[1..])
            .map(|(attribute, column)| match attribute.kind {
                AttributeKind::Text => AttributeValues::Text(column.as_string::<i32>().clone()),
                AttributeKind::Reference(_) => {
                    AttributeValues::Reference(column.as_primitive::<UInt32Type>().clone())
                }
            })
            .collect();
        let natural_keys = columns[1 + natural_key_position].as_string::<i32>().clone();
        for (attribute, values) in definition.attributes.iter().zip(&attribute_values) {
            let (AttributeKind::Reference(target), AttributeValues::Reference(values)) =
                (attribute.kind, values)
            else {
                continue;
            };
            let target_dimension = targets
                .iter()
                .find(|dimension| dimension.ulid() == target)
                .ok_or_else(|| DimensionError::MissingTarget {
                    attribute: attribute.name.clone(),
                    target,
                })?;
            for (row, value) in values.iter().enumerate() {
                if let Some(value) = value
                    && value >= target_dimension.len()
                {
                    return Err(DimensionError::DanglingTarget {
                        attribute: attribute.name.clone(),
                        member: MemberIndex(row as u32),
                        target: MemberIndex(value),
                    });
                }
            }
        }

        let hasher = RandomState::new();
        let rows = members.num_rows();
        let mut by_ulid = HashTable::with_capacity(rows);
        let mut by_natural_key = HashTable::with_capacity(rows);
        for row in 0..rows {
            let member = MemberIndex(row as u32);
            if ulids.is_null(row) {
                return Err(DimensionError::NullUlid(member));
            }
            if natural_keys.is_null(row) {
                return Err(DimensionError::NullNaturalKey(member));
            }
            let ulid = ulid_at(&ulids, row);
            match by_ulid.entry(
                hasher.hash_one(ulid),
                |other: &MemberIndex| ulid_at(&ulids, other.0 as usize) == ulid,
                |other: &MemberIndex| hasher.hash_one(ulid_at(&ulids, other.0 as usize)),
            ) {
                Entry::Occupied(_) => {
                    return Err(DimensionError::DuplicateUlid(MemberGlobalId(ulid)));
                }
                Entry::Vacant(slot) => {
                    slot.insert(member);
                }
            }
            let natural_key = natural_keys.value(row);
            match by_natural_key.entry(
                hasher.hash_one(natural_key),
                |other: &MemberIndex| natural_keys.value(other.0 as usize) == natural_key,
                |other: &MemberIndex| hasher.hash_one(natural_keys.value(other.0 as usize)),
            ) {
                Entry::Occupied(_) => {
                    return Err(DimensionError::DuplicateNaturalKey(natural_key.into()));
                }
                Entry::Vacant(slot) => {
                    slot.insert(member);
                }
            }
        }

        Ok(Self {
            definition,
            members,
            ulids,
            natural_key_position,
            natural_keys,
            attribute_values,
            hasher,
            by_ulid,
            by_natural_key,
        })
    }

    pub fn definition(&self) -> &DimensionDefinition {
        &self.definition
    }

    pub fn ulid(&self) -> DimensionGlobalId {
        self.definition.ulid
    }

    pub fn name(&self) -> &str {
        &self.definition.name
    }

    // Not the ULID alone: with the calendar and the version first, one period or version is one range of sorted keys.
    pub fn key_order(&self) -> (DimensionKind, DimensionGlobalId) {
        (self.definition.kind, self.definition.ulid)
    }

    pub fn len(&self) -> u32 {
        self.members.num_rows() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.members.num_rows() == 0
    }

    pub fn batch(&self) -> &RecordBatch {
        &self.members
    }

    pub fn member(&self, id: MemberIndex) -> Option<Member<'_>> {
        (id.0 < self.len()).then_some(Member {
            dimension: self,
            id,
        })
    }

    pub fn member_by_ulid(&self, ulid: MemberGlobalId) -> Option<Member<'_>> {
        let id = *self
            .by_ulid
            .find(self.hasher.hash_one(ulid.0), |id: &MemberIndex| {
                ulid_at(&self.ulids, id.0 as usize) == ulid.0
            })?;
        self.member(id)
    }

    pub fn member_by_natural_key(&self, natural_key: &str) -> Option<Member<'_>> {
        let id = *self
            .by_natural_key
            .find(self.hasher.hash_one(natural_key), |id: &MemberIndex| {
                self.natural_keys.value(id.0 as usize) == natural_key
            })?;
        self.member(id)
    }

    pub fn natural_key_attribute(&self) -> TextAttribute<'_> {
        TextAttribute {
            definition: &self.definition.attributes[self.natural_key_position],
            values: &self.natural_keys,
        }
    }

    pub fn text_attribute(&self, name: &str) -> Option<TextAttribute<'_>> {
        self.text_attributes()
            .find(|attribute| attribute.name() == name)
    }

    pub fn text_attributes(&self) -> impl Iterator<Item = TextAttribute<'_>> {
        self.definition
            .attributes
            .iter()
            .zip(&self.attribute_values)
            .filter_map(|(definition, values)| match values {
                AttributeValues::Text(values) => Some(TextAttribute { definition, values }),
                AttributeValues::Reference(_) => None,
            })
    }

    pub fn reference_attribute(&self, name: &str) -> Option<ReferenceAttribute<'_>> {
        self.reference_attributes()
            .find(|attribute| attribute.name() == name)
    }

    pub fn reference_attributes(&self) -> impl Iterator<Item = ReferenceAttribute<'_>> {
        self.definition
            .attributes
            .iter()
            .zip(&self.attribute_values)
            .filter_map(|(definition, values)| match (definition.kind, values) {
                (AttributeKind::Reference(target), AttributeValues::Reference(values)) => {
                    Some(ReferenceAttribute {
                        definition,
                        target,
                        values,
                    })
                }
                _ => None,
            })
    }
}

#[derive(Clone, Copy)]
pub struct Member<'d> {
    dimension: &'d Dimension,
    id: MemberIndex,
}

impl<'d> Member<'d> {
    pub fn id(self) -> MemberIndex {
        self.id
    }

    pub fn ulid(self) -> MemberGlobalId {
        MemberGlobalId(ulid_at(&self.dimension.ulids, self.id.0 as usize))
    }

    pub fn natural_key(self) -> &'d str {
        self.dimension.natural_keys.value(self.id.0 as usize)
    }
}

#[derive(Clone, Copy)]
pub struct TextAttribute<'d> {
    definition: &'d AttributeDefinition,
    values: &'d StringArray,
}

impl<'d> TextAttribute<'d> {
    pub fn ulid(self) -> AttributeGlobalId {
        self.definition.ulid
    }

    pub fn name(self) -> &'d str {
        &self.definition.name
    }

    pub fn value(self, member: MemberIndex) -> Option<&'d str> {
        let row = member.0 as usize;
        (row < self.values.len() && self.values.is_valid(row)).then(|| self.values.value(row))
    }
}

#[derive(Clone, Copy)]
pub struct ReferenceAttribute<'d> {
    definition: &'d AttributeDefinition,
    target: DimensionGlobalId,
    values: &'d UInt32Array,
}

impl<'d> ReferenceAttribute<'d> {
    pub fn ulid(self) -> AttributeGlobalId {
        self.definition.ulid
    }

    pub fn name(self) -> &'d str {
        &self.definition.name
    }

    pub fn target(self) -> DimensionGlobalId {
        self.target
    }

    pub fn value(self, member: MemberIndex) -> Option<MemberIndex> {
        let row = member.0 as usize;
        (row < self.values.len() && self.values.is_valid(row))
            .then(|| MemberIndex(self.values.value(row)))
    }

    pub fn values(self) -> &'d UInt32Array {
        self.values
    }
}

impl fmt::Debug for Member<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("id", &self.id)
            .field("ulid", &self.ulid())
            .finish()
    }
}

impl fmt::Debug for TextAttribute<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.definition, f)
    }
}

impl fmt::Debug for ReferenceAttribute<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.definition, f)
    }
}

fn ulid_at(ulids: &FixedSizeBinaryArray, row: usize) -> Ulid {
    let bytes: [u8; 16] = ulids
        .value(row)
        .try_into()
        .expect("FixedSizeBinary(16) values are 16 bytes");
    Ulid::from_bytes(bytes)
}
