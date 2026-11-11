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
const NAME_COLUMN: &str = "name";
const CODE_COLUMN: &str = "code";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DimensionGlobalId(pub Ulid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberGlobalId(pub Ulid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttributeGlobalId(pub Ulid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberIndex(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Name,
    Code,
}

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
    pub target: DimensionGlobalId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DimensionDefinition {
    pub ulid: DimensionGlobalId,
    pub name: String,
    pub kind: DimensionKind,
    pub key_kind: KeyKind,
    pub attributes: Vec<AttributeDefinition>,
}

impl DimensionDefinition {
    pub fn schema(&self) -> Schema {
        let mut fields = vec![
            Field::new(ULID_COLUMN, DataType::FixedSizeBinary(16), false),
            Field::new(NAME_COLUMN, DataType::Utf8, false),
        ];
        if self.key_kind == KeyKind::Code {
            fields.push(Field::new(CODE_COLUMN, DataType::Utf8, false));
        }
        for attribute in &self.attributes {
            fields.push(Field::new(
                attribute.ulid.0.to_string(),
                DataType::UInt32,
                true,
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
    names: StringArray,
    keys: StringArray,
    attribute_values: Vec<UInt32Array>,
    hasher: RandomState,
    // Not HashMap<Ulid, MemberIndex>: the ULID and key bytes already live in the Arrow arrays.
    by_ulid: HashTable<MemberIndex>,
    by_key: HashTable<MemberIndex>,
}

#[derive(Debug, PartialEq)]
pub enum DimensionError {
    TooManyMembers(usize),
    DuplicateAttributeName(String),
    DuplicateAttributeUlid(AttributeGlobalId),
    AttributeTargetsItself(String),
    SchemaMismatch {
        expected: SchemaRef,
        found: SchemaRef,
    },
    NullUlid(MemberIndex),
    DuplicateUlid(MemberGlobalId),
    NullName(MemberIndex),
    NullKey(MemberIndex),
    DuplicateKey(String),
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
            if attribute.target == definition.ulid {
                return Err(DimensionError::AttributeTargetsItself(
                    attribute.name.clone(),
                ));
            }
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
        let names = columns[1].as_string::<i32>().clone();
        let (keys, attribute_columns) = match definition.key_kind {
            KeyKind::Name => (names.clone(), &columns[2..]),
            KeyKind::Code => (columns[2].as_string::<i32>().clone(), &columns[3..]),
        };
        let attribute_values: Vec<UInt32Array> = attribute_columns
            .iter()
            .map(|column| column.as_primitive::<UInt32Type>().clone())
            .collect();
        for (attribute, values) in definition.attributes.iter().zip(&attribute_values) {
            let target = targets
                .iter()
                .find(|target| target.ulid() == attribute.target)
                .ok_or_else(|| DimensionError::MissingTarget {
                    attribute: attribute.name.clone(),
                    target: attribute.target,
                })?;
            for (row, value) in values.iter().enumerate() {
                if let Some(value) = value
                    && value >= target.len()
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
        let mut by_key = HashTable::with_capacity(rows);
        for row in 0..rows {
            let member = MemberIndex(row as u32);
            if ulids.is_null(row) {
                return Err(DimensionError::NullUlid(member));
            }
            if names.is_null(row) {
                return Err(DimensionError::NullName(member));
            }
            if keys.is_null(row) {
                return Err(DimensionError::NullKey(member));
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
            let key = keys.value(row);
            match by_key.entry(
                hasher.hash_one(key),
                |other: &MemberIndex| keys.value(other.0 as usize) == key,
                |other: &MemberIndex| hasher.hash_one(keys.value(other.0 as usize)),
            ) {
                Entry::Occupied(_) => return Err(DimensionError::DuplicateKey(key.into())),
                Entry::Vacant(slot) => {
                    slot.insert(member);
                }
            }
        }

        Ok(Self {
            definition,
            members,
            ulids,
            names,
            keys,
            attribute_values,
            hasher,
            by_ulid,
            by_key,
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

    pub fn key_kind(&self) -> KeyKind {
        self.definition.key_kind
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

    pub fn member_by_key(&self, key: &str) -> Option<Member<'_>> {
        let id = *self
            .by_key
            .find(self.hasher.hash_one(key), |id: &MemberIndex| {
                self.keys.value(id.0 as usize) == key
            })?;
        self.member(id)
    }

    pub fn attribute(&self, name: &str) -> Option<Attribute<'_>> {
        self.attributes().find(|attribute| attribute.name() == name)
    }

    pub fn attributes(&self) -> impl ExactSizeIterator<Item = Attribute<'_>> {
        self.definition
            .attributes
            .iter()
            .zip(&self.attribute_values)
            .map(|(definition, values)| Attribute { definition, values })
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

    pub fn name(self) -> &'d str {
        self.dimension.names.value(self.id.0 as usize)
    }

    pub fn key(self) -> &'d str {
        self.dimension.keys.value(self.id.0 as usize)
    }
}

#[derive(Clone, Copy)]
pub struct Attribute<'d> {
    definition: &'d AttributeDefinition,
    values: &'d UInt32Array,
}

impl<'d> Attribute<'d> {
    pub fn ulid(self) -> AttributeGlobalId {
        self.definition.ulid
    }

    pub fn name(self) -> &'d str {
        &self.definition.name
    }

    pub fn target(self) -> DimensionGlobalId {
        self.definition.target
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

impl fmt::Debug for Attribute<'_> {
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
