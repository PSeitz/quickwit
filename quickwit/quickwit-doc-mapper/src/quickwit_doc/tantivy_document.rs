// Copyright 2021-Present Datadog, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::slice;

use tantivy::schema::document::{ReferenceValue, ReferenceValueLeaf};
use tantivy::schema::{Document, Field, Value};
use tantivy::time::format_description::well_known::Rfc3339;
use tantivy::time::{OffsetDateTime, UtcOffset};

use super::{
    ByteDictionary, QuickwitArray, QuickwitDoc, QuickwitLeafValue, QuickwitObject, QuickwitValue,
    decode_value, read_u32,
};

/// Tantivy fields bound to original offsets, converted values, or the dynamic projection.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FieldBindings {
    values: Vec<(Field, Binding)>,
    dynamic: Vec<DynamicEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Binding {
    Original { offset: u32, is_json: bool },
    Computed(QuickwitLeafValue),
    Dynamic,
}

/// Pre-order entry describing a `_dynamic` projection over `BatchData::nodes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DynamicEntry {
    /// Whole `(key_id, value)` starting at `entry_offset` in `BatchData::nodes`.
    Whole(u32),
    /// `(key_id, partial_object)` whose children occupy the next `subtree_len` entries.
    PartialObject { key_id: u32, subtree_len: u32 },
}

impl FieldBindings {
    #[inline]
    pub(crate) fn bind(&mut self, field: Field, offset: u32, is_json: bool) {
        self.values
            .push((field, Binding::Original { offset, is_json }));
    }

    #[inline]
    pub(crate) fn bind_computed(&mut self, field: Field, value: QuickwitLeafValue) {
        self.values.push((field, Binding::Computed(value)));
    }

    #[inline]
    pub(crate) fn push_dynamic_whole(&mut self, entry_offset: u32) {
        self.dynamic.push(DynamicEntry::Whole(entry_offset));
    }

    #[inline]
    pub(crate) fn begin_dynamic_object(&mut self, key_id: u32) -> usize {
        let index = self.dynamic.len();
        self.dynamic.push(DynamicEntry::PartialObject {
            key_id,
            subtree_len: 0,
        });
        index
    }

    #[inline]
    pub(crate) fn end_dynamic_object(&mut self, header_index: usize) {
        let subtree_len = self.dynamic.len() - header_index - 1;
        if subtree_len == 0 {
            self.dynamic.truncate(header_index);
        } else if let DynamicEntry::PartialObject {
            subtree_len: slot, ..
        } = &mut self.dynamic[header_index]
        {
            *slot = subtree_len as u32;
        }
    }

    pub(crate) fn bind_dynamic(&mut self, field: Field) {
        if !self.dynamic.is_empty() {
            self.values.push((field, Binding::Dynamic));
        }
    }
}

/// Yields Tantivy field IDs and their mapped views of the document.
pub struct MappedFields<'a> {
    nodes: &'a [u8],
    dictionary: &'a ByteDictionary,
    dynamic: &'a [DynamicEntry],
    bindings: slice::Iter<'a, (Field, Binding)>,
}

impl<'a> Iterator for MappedFields<'a> {
    type Item = (Field, MappedValue<'a>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let &(field, binding) = self.bindings.next()?;
        let view = match binding {
            Binding::Original { offset, is_json } => View::Value {
                value: decode_value(&self.nodes[offset as usize..], offset).0,
                is_json,
            },
            Binding::Computed(leaf) => View::Value {
                value: QuickwitValue::Leaf(leaf),
                is_json: false,
            },
            Binding::Dynamic => View::Dynamic {
                entries: self.dynamic,
                nodes: self.nodes,
            },
        };
        Some((field, MappedValue::new(view, self.dictionary)))
    }
}

impl Document for QuickwitDoc {
    type Value<'a> = MappedValue<'a>;
    type FieldsValuesIter<'a> = MappedFields<'a>;

    fn iter_fields_and_values(&self) -> Self::FieldsValuesIter<'_> {
        MappedFields {
            nodes: &self.data.nodes,
            dictionary: &self.data.dictionary,
            dynamic: &self.bindings.dynamic,
            bindings: self.bindings.values.iter(),
        }
    }
}

/// Tantivy's view of a mapped field. Typed fields keep their exact mapped type; JSON fields
/// (`_source`, `_dynamic`, json) follow Tantivy's `OwnedValue::from(serde_json::Value)`.
#[derive(Clone, Copy, Debug)]
pub struct MappedValue<'a> {
    view: View<'a>,
    dictionary: &'a ByteDictionary,
}

impl<'a> MappedValue<'a> {
    #[inline]
    fn new(view: View<'a>, dictionary: &'a ByteDictionary) -> Self {
        Self { view, dictionary }
    }

    #[inline]
    fn from_value(value: QuickwitValue<'a>, is_json: bool, dictionary: &'a ByteDictionary) -> Self {
        Self::new(View::Value { value, is_json }, dictionary)
    }
}

#[derive(Clone, Copy, Debug)]
enum View<'a> {
    Value {
        value: QuickwitValue<'a>,
        is_json: bool,
    },
    Dynamic {
        entries: &'a [DynamicEntry],
        nodes: &'a [u8],
    },
}

impl<'a> Value<'a> for MappedValue<'a> {
    type ArrayIter = MappedArray<'a>;
    type ObjectIter = MappedObject<'a>;

    #[inline]
    fn as_value(&self) -> ReferenceValue<'a, Self> {
        match self.view {
            View::Dynamic { entries, nodes } => {
                ReferenceValue::Object(MappedObject(MappedObjectInner::Dynamic {
                    entries,
                    nodes,
                    dictionary: self.dictionary,
                }))
            }
            View::Value { value, is_json } => match value {
                QuickwitValue::Leaf(leaf) => {
                    let leaf = tantivy_leaf(leaf, self.dictionary);
                    ReferenceValue::Leaf(if is_json { json_leaf(leaf) } else { leaf })
                }
                QuickwitValue::Array(array) => ReferenceValue::Array(MappedArray {
                    array,
                    dictionary: self.dictionary,
                    is_json,
                }),
                QuickwitValue::Object(object) => {
                    ReferenceValue::Object(MappedObject(MappedObjectInner::Encoded {
                        object,
                        dictionary: self.dictionary,
                        is_json,
                    }))
                }
            },
        }
    }
}

#[inline]
fn tantivy_leaf(leaf: QuickwitLeafValue, dictionary: &ByteDictionary) -> ReferenceValueLeaf<'_> {
    match leaf {
        QuickwitLeafValue::Null => ReferenceValueLeaf::Null,
        QuickwitLeafValue::Bool(value) => ReferenceValueLeaf::Bool(value),
        QuickwitLeafValue::I64(value) => ReferenceValueLeaf::I64(value),
        QuickwitLeafValue::U64(value) => ReferenceValueLeaf::U64(value),
        QuickwitLeafValue::F64(value) => ReferenceValueLeaf::F64(value),
        QuickwitLeafValue::String(id) => ReferenceValueLeaf::Str(dictionary.get_str(id)),
        QuickwitLeafValue::Bytes(id) => ReferenceValueLeaf::Bytes(dictionary.get_bytes(id)),
        QuickwitLeafValue::DateTime(value) => ReferenceValueLeaf::Date(value),
        QuickwitLeafValue::IpAddr(id) => ReferenceValueLeaf::IpAddr(dictionary.get_ip(id)),
    }
}

#[inline]
fn is_rfc3339_candidate(bytes: &[u8]) -> bool {
    bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[0].is_ascii_digit()
}

#[inline]
fn json_leaf(leaf: ReferenceValueLeaf<'_>) -> ReferenceValueLeaf<'_> {
    match leaf {
        ReferenceValueLeaf::U64(number) => i64::try_from(number)
            .map(ReferenceValueLeaf::I64)
            .unwrap_or(leaf),
        // Match Tantivy's RFC3339 parsing and nanosecond range check.
        ReferenceValueLeaf::Str(text) if is_rfc3339_candidate(text.as_bytes()) => {
            OffsetDateTime::parse(text, &Rfc3339)
                .ok()
                .and_then(|dt| tantivy::DateTime::try_from_utc(dt.to_offset(UtcOffset::UTC)))
                .map(ReferenceValueLeaf::Date)
                .unwrap_or(leaf)
        }
        _ => leaf,
    }
}

/// Borrows the elements of a mapped array in order.
pub struct MappedArray<'a> {
    array: QuickwitArray<'a>,
    dictionary: &'a ByteDictionary,
    is_json: bool,
}

impl<'a> Iterator for MappedArray<'a> {
    type Item = MappedValue<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let (_, value) = self.array.next_entry()?;
        Some(MappedValue::from_value(
            value,
            self.is_json,
            self.dictionary,
        ))
    }
}

/// Borrows all fields of a mapped object or the subset selected for `_dynamic`.
pub struct MappedObject<'a>(MappedObjectInner<'a>);

enum MappedObjectInner<'a> {
    Encoded {
        object: QuickwitObject<'a>,
        dictionary: &'a ByteDictionary,
        is_json: bool,
    },
    Dynamic {
        entries: &'a [DynamicEntry],
        nodes: &'a [u8],
        dictionary: &'a ByteDictionary,
    },
}

impl<'a> Iterator for MappedObject<'a> {
    type Item = (&'a str, MappedValue<'a>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            MappedObjectInner::Encoded {
                object,
                dictionary,
                is_json,
            } => {
                let (key_id, _, _, value) = object.next_entry()?;
                Some((
                    dictionary.get_str(key_id),
                    MappedValue::from_value(value, *is_json, dictionary),
                ))
            }
            MappedObjectInner::Dynamic {
                entries,
                nodes,
                dictionary,
            } => {
                let (&entry, rest) = entries.split_first()?;
                match entry {
                    DynamicEntry::Whole(entry_offset) => {
                        *entries = rest;
                        let key_id = read_u32(&nodes[entry_offset as usize..]);
                        let value_offset = entry_offset + 4;
                        let (value, _) =
                            decode_value(&nodes[value_offset as usize..], value_offset);
                        Some((
                            dictionary.get_str(key_id),
                            MappedValue::from_value(value, true, dictionary),
                        ))
                    }
                    DynamicEntry::PartialObject {
                        key_id,
                        subtree_len,
                    } => {
                        let (children, remaining) = rest.split_at(subtree_len as usize);
                        *entries = remaining;
                        Some((
                            dictionary.get_str(key_id),
                            MappedValue::new(
                                View::Dynamic {
                                    entries: children,
                                    nodes,
                                },
                                dictionary,
                            ),
                        ))
                    }
                }
            }
        }
    }
}
