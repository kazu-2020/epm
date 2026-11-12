use std::collections::HashSet;
use std::fmt;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::UInt32Type;
use arrow_array::{Array, ArrayRef, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array};
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
    ulid: DimensionGlobalId,
    name: String,
    kind: DimensionKind,
    attributes: Vec<AttributeDefinition>,
    natural_key_position: usize,
}

#[derive(Debug, PartialEq)]
pub enum DefinitionError {
    DuplicateAttributeName(String),
    DuplicateAttributeUlid(AttributeGlobalId),
    AttributeTargetsItself(String),
    MissingNaturalKey(AttributeGlobalId),
    NaturalKeyIsNotATextAttribute(AttributeGlobalId),
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for DefinitionError {}

impl DimensionDefinition {
    pub fn try_new(
        ulid: DimensionGlobalId,
        name: String,
        kind: DimensionKind,
        natural_key_attribute: AttributeGlobalId,
        attributes: Vec<AttributeDefinition>,
    ) -> Result<Self, DefinitionError> {
        let mut attribute_names = HashSet::new();
        let mut attribute_ulids = HashSet::new();
        for attribute in &attributes {
            if !attribute_names.insert(attribute.name.as_str()) {
                return Err(DefinitionError::DuplicateAttributeName(
                    attribute.name.clone(),
                ));
            }
            if !attribute_ulids.insert(attribute.ulid) {
                return Err(DefinitionError::DuplicateAttributeUlid(attribute.ulid));
            }
            if attribute.kind == AttributeKind::Reference(ulid) {
                return Err(DefinitionError::AttributeTargetsItself(
                    attribute.name.clone(),
                ));
            }
        }
        let natural_key_position = attributes
            .iter()
            .position(|attribute| attribute.ulid == natural_key_attribute)
            .ok_or(DefinitionError::MissingNaturalKey(natural_key_attribute))?;
        if attributes[natural_key_position].kind != AttributeKind::Text {
            return Err(DefinitionError::NaturalKeyIsNotATextAttribute(
                natural_key_attribute,
            ));
        }
        Ok(Self {
            ulid,
            name,
            kind,
            attributes,
            natural_key_position,
        })
    }

    pub fn ulid(&self) -> DimensionGlobalId {
        self.ulid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> DimensionKind {
        self.kind
    }

    pub fn attributes(&self) -> &[AttributeDefinition] {
        &self.attributes
    }

    pub fn natural_key_attribute(&self) -> &AttributeDefinition {
        &self.attributes[self.natural_key_position]
    }

    pub fn schema(&self) -> Schema {
        let mut fields = vec![Field::new(
            ULID_COLUMN,
            DataType::FixedSizeBinary(16),
            false,
        )];
        for (position, attribute) in self.attributes.iter().enumerate() {
            let data_type = match attribute.kind {
                AttributeKind::Text => DataType::Utf8,
                AttributeKind::Reference(_) => DataType::UInt32,
            };
            fields.push(Field::new(
                attribute.ulid.0.to_string(),
                data_type,
                position != self.natural_key_position,
            ));
        }
        Schema::new(fields)
    }
}

#[derive(Debug)]
pub struct Dimension {
    definition: DimensionDefinition,
    members: RecordBatch,
    by_ulid: UniqueIndex,
    by_natural_key: UniqueIndex,
}

// Not HashMap<Ulid, MemberIndex>: the ULID and Natural Key bytes already live in the Arrow arrays.
#[derive(Debug, Default)]
struct UniqueIndex {
    hasher: RandomState,
    rows: HashTable<MemberIndex>,
}

impl UniqueIndex {
    fn new<K: Hash + Eq>(rows: usize, key: impl Fn(usize) -> K) -> Result<Self, MemberIndex> {
        let hasher = RandomState::new();
        let mut index = HashTable::with_capacity(rows);
        for row in 0..rows {
            let value = key(row);
            match index.entry(
                hasher.hash_one(&value),
                |other: &MemberIndex| key(other.0 as usize) == value,
                |other: &MemberIndex| hasher.hash_one(key(other.0 as usize)),
            ) {
                Entry::Occupied(_) => return Err(MemberIndex(row as u32)),
                Entry::Vacant(slot) => {
                    slot.insert(MemberIndex(row as u32));
                }
            }
        }
        Ok(Self {
            hasher,
            rows: index,
        })
    }

    fn find<K: Hash + Eq>(&self, wanted: K, key: impl Fn(usize) -> K) -> Option<MemberIndex> {
        self.rows
            .find(self.hasher.hash_one(&wanted), |row| {
                key(row.0 as usize) == wanted
            })
            .copied()
    }
}

#[derive(Debug, PartialEq)]
pub enum DimensionError {
    TooManyMembers(usize),
    SchemaMismatch {
        expected: SchemaRef,
        found: SchemaRef,
    },
    NullUlid(MemberIndex),
    DuplicateUlid(MemberGlobalId),
    NullNaturalKey(MemberIndex),
    EmptyNaturalKey(MemberIndex),
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
        let rows = members.num_rows();
        u32::try_from(rows).map_err(|_| DimensionError::TooManyMembers(rows))?;
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
        let mut dimension = Self {
            definition,
            members,
            by_ulid: UniqueIndex::default(),
            by_natural_key: UniqueIndex::default(),
        };

        for attribute in dimension.reference_attributes() {
            let target_dimension = targets
                .iter()
                .find(|candidate| candidate.ulid() == attribute.target)
                .ok_or_else(|| DimensionError::MissingTarget {
                    attribute: attribute.name().into(),
                    target: attribute.target,
                })?;
            for (row, value) in attribute.values.iter().enumerate() {
                if let Some(value) = value
                    && value >= target_dimension.len()
                {
                    return Err(DimensionError::DanglingTarget {
                        attribute: attribute.name().into(),
                        member: MemberIndex(row as u32),
                        target: MemberIndex(value),
                    });
                }
            }
        }

        let ulids = dimension.ulids();
        let natural_keys = dimension.natural_keys();
        for row in 0..rows {
            let member = MemberIndex(row as u32);
            if ulids.is_null(row) {
                return Err(DimensionError::NullUlid(member));
            }
            if natural_keys.is_null(row) {
                return Err(DimensionError::NullNaturalKey(member));
            }
            if natural_keys.value(row).is_empty() {
                return Err(DimensionError::EmptyNaturalKey(member));
            }
        }
        let by_ulid = UniqueIndex::new(rows, |row| ulid_at(ulids, row)).map_err(|member| {
            DimensionError::DuplicateUlid(MemberGlobalId(ulid_at(ulids, member.0 as usize)))
        })?;
        let by_natural_key =
            UniqueIndex::new(rows, |row| natural_keys.value(row)).map_err(|member| {
                DimensionError::DuplicateNaturalKey(natural_keys.value(member.0 as usize).into())
            })?;
        dimension.by_ulid = by_ulid;
        dimension.by_natural_key = by_natural_key;
        Ok(dimension)
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
        let id = self
            .by_ulid
            .find(ulid.0, |row| ulid_at(self.ulids(), row))?;
        self.member(id)
    }

    pub fn member_by_natural_key(&self, natural_key: &str) -> Option<Member<'_>> {
        let id = self
            .by_natural_key
            .find(natural_key, |row| self.natural_keys().value(row))?;
        self.member(id)
    }

    pub fn natural_key_attribute(&self) -> TextAttribute<'_> {
        TextAttribute {
            definition: self.definition.natural_key_attribute(),
            values: self.natural_keys(),
        }
    }

    pub fn text_attribute(&self, name: &str) -> Option<TextAttribute<'_>> {
        self.text_attributes()
            .find(|attribute| attribute.name() == name)
    }

    pub fn text_attributes(&self) -> impl Iterator<Item = TextAttribute<'_>> {
        self.attribute_columns()
            .filter_map(|(definition, column)| match definition.kind {
                AttributeKind::Text => Some(TextAttribute {
                    definition,
                    values: column.as_string::<i32>(),
                }),
                AttributeKind::Reference(_) => None,
            })
    }

    pub fn reference_attribute(&self, name: &str) -> Option<ReferenceAttribute<'_>> {
        self.reference_attributes()
            .find(|attribute| attribute.name() == name)
    }

    pub fn reference_attributes(&self) -> impl Iterator<Item = ReferenceAttribute<'_>> {
        self.attribute_columns()
            .filter_map(|(definition, column)| match definition.kind {
                AttributeKind::Reference(target) => Some(ReferenceAttribute {
                    definition,
                    target,
                    values: column.as_primitive::<UInt32Type>(),
                }),
                AttributeKind::Text => None,
            })
    }

    fn attribute_columns(&self) -> impl Iterator<Item = (&AttributeDefinition, &ArrayRef)> {
        self.definition
            .attributes
            .iter()
            .zip(&self.members.columns()[1..])
    }

    fn ulids(&self) -> &FixedSizeBinaryArray {
        self.members.column(0).as_fixed_size_binary()
    }

    fn natural_keys(&self) -> &StringArray {
        self.members
            .column(1 + self.definition.natural_key_position)
            .as_string::<i32>()
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
        MemberGlobalId(ulid_at(self.dimension.ulids(), self.id.0 as usize))
    }

    pub fn natural_key(self) -> &'d str {
        self.dimension.natural_keys().value(self.id.0 as usize)
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
