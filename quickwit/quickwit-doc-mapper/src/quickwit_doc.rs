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

//! Batch-interned documents stored in a flat byte buffer (`BatchData::nodes`).
//!
//! Layout per value:
//! - `TAG_NULL` / `TAG_FALSE` / `TAG_TRUE`: 1 byte
//! - `TAG_STRING`: tag + `dict_id: u32` LE (5 bytes)
//! - `TAG_I64` / `TAG_U64` / `TAG_F64`: tag + 8-byte LE payload (9 bytes)
//! - `TAG_ARRAY` / `TAG_OBJECT`: tag + `len: u32` + `body_len: u32` + body bytes
//!
//! Object bodies store `len` `(key_id: u32, value)` entries sorted by `key_id`. Field names are
//! literal (`a.b` is one key).

mod dictionary;
mod parse;
mod tantivy_document;
#[cfg(test)]
mod tests;

use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub use dictionary::ByteDictionary;
pub use parse::QuickwitDocBatch;
use serde_json::Number;
pub(crate) use tantivy_document::FieldBindings;

use crate::routing_expression::RoutingExprContext;

pub(crate) const TAG_NULL: u8 = 0;
pub(crate) const TAG_FALSE: u8 = 1;
pub(crate) const TAG_TRUE: u8 = 2;
pub(crate) const TAG_I64: u8 = 3;
pub(crate) const TAG_U64: u8 = 4;
pub(crate) const TAG_F64: u8 = 5;
pub(crate) const TAG_STRING: u8 = 6;
pub(crate) const TAG_ARRAY: u8 = 7;
pub(crate) const TAG_OBJECT: u8 = 8;

const CONTAINER_HEADER_LEN: usize = 9;

#[inline]
fn read_bytes<const N: usize>(bytes: &[u8]) -> [u8; N] {
    bytes[..N].try_into().unwrap()
}

#[inline]
pub(crate) fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(read_bytes(bytes))
}

#[derive(Debug, Default)]
pub(crate) struct BatchData {
    pub(crate) dictionary: ByteDictionary,
    pub(crate) nodes: Vec<u8>,
}

impl BatchData {
    fn mem_usage(&self) -> usize {
        std::mem::size_of::<Self>() + self.dictionary.mem_usage() + self.nodes.capacity()
    }
}

/// A parsed document referencing its root object inside the shared batch buffer, plus Tantivy
/// field bindings populated during mapping.
#[derive(Clone, Debug, Default)]
pub struct QuickwitDoc {
    pub(crate) root_offset: u32,
    pub(crate) data: Arc<BatchData>,
    pub(crate) bindings: FieldBindings,
}

/// A borrowed document value. Field names and leaf IDs refer to the document's dictionary.
#[derive(Clone, Copy, Debug)]
pub enum QuickwitValue<'a> {
    /// A value without children.
    Leaf(QuickwitLeafValue),
    /// An ordered sequence of values.
    Array(QuickwitArray<'a>),
    /// A nested object sharing the root's dictionary.
    Object(QuickwitObject<'a>),
}

/// A scalar stored inline or by ID in the document's byte dictionary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuickwitLeafValue {
    /// A JSON null.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    I64(i64),
    /// An unsigned integer.
    U64(u64),
    /// A floating-point number.
    F64(f64),
    /// UTF-8 text in the batch dictionary.
    String(u32),
    /// Decoded binary data in the batch dictionary.
    Bytes(u32),
    /// A parsed timestamp.
    DateTime(tantivy::DateTime),
    /// Dictionary ID of 16 network-order octets (IPv4 uses IPv4-mapped IPv6).
    IpAddr(u32),
}

impl QuickwitDoc {
    /// The root fields.
    #[inline]
    pub fn as_object(&self) -> QuickwitObject<'_> {
        decode_root_object(&self.data.nodes, self.root_offset)
    }

    /// Resolves all dictionary IDs in this document.
    #[inline]
    pub fn dictionary(&self) -> &ByteDictionary {
        &self.data.dictionary
    }

    /// Looks up a value by literal object keys.
    pub fn get_path(&self, path: &[String]) -> Option<QuickwitValue<'_>> {
        self.as_object().get_path(path, &self.data.dictionary)
    }

    /// Heap capacity owned exclusively by this document's Tantivy field bindings.
    pub fn bindings_mem_usage(&self) -> usize {
        self.bindings.mem_usage()
    }

    /// Identity and retained heap size of the batch buffer shared across documents from the same
    /// [`QuickwitDocBatch`]. Callers retaining multiple documents can deduplicate by the returned
    /// address so shared batch buffers are counted once.
    pub fn batch_data_id_and_mem_usage(&self) -> (usize, usize) {
        (Arc::as_ptr(&self.data) as usize, self.data.mem_usage())
    }
}

impl QuickwitValue<'_> {
    /// Hashes the input type, sizes, and contents without constructing a JSON tree. Routing and
    /// clustering use the same type tags but encode lengths and JSON numbers differently.
    pub fn hash_json<H: Hasher>(
        &self,
        dictionary: &ByteDictionary,
        hasher: &mut H,
        write_len: impl Fn(&mut H, usize) + Copy,
        write_number: impl Fn(&mut H, &Number) + Copy,
    ) {
        let mut number = |value: Number| {
            hasher.write_u8(2);
            write_number(hasher, &value);
        };
        match *self {
            Self::Leaf(QuickwitLeafValue::Null) => hasher.write_u8(0),
            Self::Leaf(QuickwitLeafValue::Bool(value)) => {
                hasher.write_u8(1);
                value.hash(hasher);
            }
            Self::Leaf(QuickwitLeafValue::I64(value)) => number(Number::from(value)),
            Self::Leaf(QuickwitLeafValue::U64(value)) => number(Number::from(value)),
            Self::Leaf(QuickwitLeafValue::F64(value)) => {
                if let Some(value) = Number::from_f64(value) {
                    number(value);
                } else {
                    hasher.write_u8(9);
                    hasher.write_u64(value.to_bits());
                }
            }
            Self::Leaf(QuickwitLeafValue::String(id)) => {
                let bytes = dictionary.get_bytes(id);
                hasher.write_u8(3);
                write_len(hasher, bytes.len());
                hasher.write(bytes);
            }
            Self::Array(values) => {
                hasher.write_u8(4);
                write_len(hasher, values.len());
                for value in values.iter() {
                    value.hash_json(dictionary, hasher, write_len, write_number);
                }
            }
            Self::Object(object) => {
                hasher.write_u8(5);
                write_len(hasher, object.len());
                let mut entries: Vec<(&[u8], QuickwitValue<'_>)> = object
                    .entries()
                    .map(|(id, _, _, value)| (dictionary.get_bytes(id), value))
                    .collect();
                entries.sort_unstable_by_key(|(key, _)| *key);
                for (key, value) in entries {
                    write_len(hasher, key.len());
                    hasher.write(key);
                    value.hash_json(dictionary, hasher, write_len, write_number);
                }
            }
            Self::Leaf(
                QuickwitLeafValue::Bytes(_)
                | QuickwitLeafValue::DateTime(_)
                | QuickwitLeafValue::IpAddr(_),
            ) => unreachable!("parsed documents only contain JSON values"),
        }
    }
}

impl RoutingExprContext for QuickwitDoc {
    fn hash_attribute<H: Hasher>(&self, path: &[String], hasher: &mut H) {
        let Some(value) = self.get_path(path) else {
            hasher.write_u8(0);
            return;
        };
        hasher.write_u8(1);
        value.hash_json(
            self.dictionary(),
            hasher,
            |hasher, len| hasher.write_u64(len as u64),
            |hasher, number| number.hash(hasher),
        );
    }
}

#[inline]
pub(crate) fn decode_value<'a>(slice: &'a [u8], offset: u32) -> (QuickwitValue<'a>, usize) {
    use QuickwitLeafValue as Leaf;
    match slice[0] {
        TAG_NULL => (QuickwitValue::Leaf(Leaf::Null), 1),
        TAG_FALSE => (QuickwitValue::Leaf(Leaf::Bool(false)), 1),
        TAG_TRUE => (QuickwitValue::Leaf(Leaf::Bool(true)), 1),
        TAG_I64 => (
            QuickwitValue::Leaf(Leaf::I64(i64::from_le_bytes(read_bytes(&slice[1..])))),
            9,
        ),
        TAG_U64 => (
            QuickwitValue::Leaf(Leaf::U64(u64::from_le_bytes(read_bytes(&slice[1..])))),
            9,
        ),
        TAG_F64 => (
            QuickwitValue::Leaf(Leaf::F64(f64::from_le_bytes(read_bytes(&slice[1..])))),
            9,
        ),
        TAG_STRING => (QuickwitValue::Leaf(Leaf::String(read_u32(&slice[1..]))), 5),
        TAG_ARRAY | TAG_OBJECT => {
            let len = read_u32(&slice[1..]);
            let body_len = read_u32(&slice[5..]) as usize;
            let body = &slice[CONTAINER_HEADER_LEN..CONTAINER_HEADER_LEN + body_len];
            let body_offset = offset + CONTAINER_HEADER_LEN as u32;
            let value = if slice[0] == TAG_ARRAY {
                QuickwitValue::Array(QuickwitArray {
                    body,
                    body_offset,
                    len,
                })
            } else {
                QuickwitValue::Object(QuickwitObject {
                    body,
                    body_offset,
                    len,
                })
            };
            (value, CONTAINER_HEADER_LEN + body_len)
        }
        tag => unreachable!("invalid QuickwitDoc value tag: {tag}"),
    }
}

#[inline]
pub(crate) fn decode_root_object(nodes: &[u8], root_offset: u32) -> QuickwitObject<'_> {
    if nodes.is_empty() {
        return QuickwitObject {
            body: &[],
            body_offset: 0,
            len: 0,
        };
    }
    match decode_value(&nodes[root_offset as usize..], root_offset).0 {
        QuickwitValue::Object(object) => object,
        _ => unreachable!("QuickwitDoc root must be an object"),
    }
}

/// Borrowed view of an encoded object whose fields are stored in `key_id` order.
#[derive(Clone, Copy, Debug)]
pub struct QuickwitObject<'a> {
    body: &'a [u8],
    body_offset: u32,
    len: u32,
}

impl<'a> QuickwitObject<'a> {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len as usize
    }

    /// Iterates over literal field names and values.
    #[inline]
    pub fn iter(
        self,
        dictionary: &'a ByteDictionary,
    ) -> impl Iterator<Item = (&'a str, QuickwitValue<'a>)> + 'a {
        self.entries()
            .map(move |(key_id, _, _, value)| (dictionary.get_str(key_id), value))
    }

    #[inline]
    pub(crate) fn next_entry(&mut self) -> Option<(u32, u32, u32, QuickwitValue<'a>)> {
        if self.body.is_empty() {
            return None;
        }
        let key_id = read_u32(self.body);
        let entry_offset = self.body_offset;
        let value_offset = entry_offset + 4;
        let (value, value_size) = decode_value(&self.body[4..], value_offset);
        let step = 4 + value_size;
        self.body = &self.body[step..];
        self.body_offset += step as u32;
        Some((key_id, entry_offset, value_offset, value))
    }

    #[inline]
    pub(crate) fn entries(
        mut self,
    ) -> impl Iterator<Item = (u32, u32, u32, QuickwitValue<'a>)> + 'a {
        std::iter::from_fn(move || self.next_entry())
    }

    /// Looks up a literal field name in this object.
    pub fn get(self, name: &str, dictionary: &ByteDictionary) -> Option<QuickwitValue<'a>> {
        let target = name.as_bytes();
        self.entries()
            .find(|&(key_id, ..)| dictionary.get_bytes(key_id) == target)
            .map(|(_, _, _, value)| value)
    }

    /// Looks up a path of literal object keys. Arrays are leaves.
    pub fn get_path(
        self,
        path: &[String],
        dictionary: &ByteDictionary,
    ) -> Option<QuickwitValue<'a>> {
        let (first, rest) = path.split_first()?;
        let mut value = self.get(first, dictionary)?;
        for key in rest {
            let QuickwitValue::Object(obj) = value else {
                return None;
            };
            value = obj.get(key, dictionary)?;
        }
        Some(value)
    }
}

/// Borrowed view of an encoded JSON array.
#[derive(Clone, Copy, Debug)]
pub struct QuickwitArray<'a> {
    body: &'a [u8],
    body_offset: u32,
    len: u32,
}

impl<'a> QuickwitArray<'a> {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len as usize
    }

    /// Returns the first element of the array, if non-empty.
    #[inline]
    pub fn first(mut self) -> Option<QuickwitValue<'a>> {
        self.next_entry().map(|(_, value)| value)
    }

    #[inline]
    pub(crate) fn iter(self) -> impl Iterator<Item = QuickwitValue<'a>> + 'a {
        self.entries().map(|(_, value)| value)
    }

    #[inline]
    pub(crate) fn next_entry(&mut self) -> Option<(u32, QuickwitValue<'a>)> {
        if self.body.is_empty() {
            return None;
        }
        let value_offset = self.body_offset;
        let (value, value_size) = decode_value(self.body, value_offset);
        self.body = &self.body[value_size..];
        self.body_offset += value_size as u32;
        Some((value_offset, value))
    }

    #[inline]
    pub(crate) fn entries(mut self) -> impl Iterator<Item = (u32, QuickwitValue<'a>)> + 'a {
        std::iter::from_fn(move || self.next_entry())
    }
}
