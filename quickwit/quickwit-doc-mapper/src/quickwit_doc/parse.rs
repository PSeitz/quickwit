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

use std::sync::Arc;

use serde::Serialize;
use serde::de::Error;
use simd_json::value::tape::Node;
use simd_json::{Buffers, INPUT_PADDING, StaticNode, Tape, fill_tape_padded};

use super::{
    BatchData, ByteDictionary, FieldBindings, QuickwitDoc, TAG_ARRAY, TAG_F64, TAG_FALSE, TAG_I64,
    TAG_NULL, TAG_OBJECT, TAG_STRING, TAG_TRUE, TAG_U64, decode_root_object,
};
use crate::doc_mapper::Partition;
use crate::{DocMapper, DocParsingError};

// Release temporary parser scratch spikes after a batch containing an oversized document.
const MAX_RETAINED_SCRATCH_CAPACITY: usize = 256 * 1024;

// The input length stays outside the result so rejected documents count towards byte metrics.
type MappedEntry<T> = (usize, Result<(T, Partition, QuickwitDoc), DocParsingError>);

/// Parses and maps JSON documents into one flat byte buffer with a shared batch dictionary.
pub struct QuickwitDocBatch {
    pub(super) data: Arc<BatchData>,
    pub(super) docs: Vec<(usize, u32)>,
    scratch: Vec<u8>,
    simd_buffers: Buffers,
    tape: Tape<'static>,
    key_scratch: Vec<u64>,
}

impl Default for QuickwitDocBatch {
    fn default() -> Self {
        Self {
            data: Arc::default(),
            docs: Vec::new(),
            scratch: Vec::new(),
            simd_buffers: Buffers::default(),
            tape: Tape(Vec::new()),
            key_scratch: Vec::new(),
        }
    }
}

impl QuickwitDocBatch {
    /// Appends a JSON object from bytes. Only whitespace may follow the object.
    pub fn parse(&mut self, input: &[u8]) -> serde_json::Result<()> {
        let len = input.len();
        self.scratch.clear();
        self.scratch.reserve(len + INPUT_PADDING);
        self.scratch.extend_from_slice(input);
        self.parse_scratch(len, len)
    }

    /// Serializes a JSON object value into the batch scratch buffer and parses it.
    pub fn push_serialize(
        &mut self,
        value: &impl Serialize,
        original_len: usize,
    ) -> serde_json::Result<()> {
        self.scratch.clear();
        serde_json::to_writer(&mut self.scratch, value)?;
        let len = self.scratch.len();
        self.parse_scratch(len, original_len)
    }

    /// Appends a document with a single `plain_text` field, retaining the UTF-8 byte length.
    pub fn push_plain_text(&mut self, text: &str) -> serde_json::Result<()> {
        push_doc(
            &mut self.data,
            &mut self.docs,
            text.len(),
            |dictionary, out| {
                let key = dictionary.intern_str("plain_text").map_err(Error::custom)?;
                let val = dictionary.intern_value_str(text).map_err(Error::custom)?;
                write_container(out, TAG_OBJECT, 1, |out| {
                    out.extend_from_slice(&key.to_le_bytes());
                    out.push(TAG_STRING);
                    out.extend_from_slice(&val.to_le_bytes());
                    Ok(())
                })
            },
        )
    }

    fn parse_scratch(&mut self, json_len: usize, original_len: usize) -> serde_json::Result<()> {
        self.scratch.resize(json_len + INPUT_PADDING, 0);
        let mut tape = std::mem::replace(&mut self.tape, Tape(Vec::new())).reset();
        let result = fill_tape_padded(
            &mut self.scratch,
            json_len,
            &mut self.simd_buffers,
            &mut tape,
        )
        .map_err(Error::custom)
        .and_then(|()| {
            let mut cursor = 0;
            self.key_scratch.clear();
            push_doc(
                &mut self.data,
                &mut self.docs,
                original_len,
                |dictionary, out| {
                    write_tape_object(
                        &tape.0,
                        &mut cursor,
                        dictionary,
                        out,
                        &mut self.key_scratch,
                    )?;
                    if cursor != tape.0.len() {
                        return Err(Error::custom("trailing characters"));
                    }
                    Ok(())
                },
            )
        });
        self.tape = tape.reset();
        result
    }

    /// Maps all roots before freezing the dictionary so identical binary conversions across
    /// documents share an entry. `before_map` reads original fields (e.g. to fingerprint them).
    /// Resets the batch while retaining scratch buffers for reuse across batches.
    pub fn map<'a, T, F: FnMut(&QuickwitDoc) -> T>(
        &mut self,
        mapper: &'a DocMapper,
        mut before_map: F,
    ) -> impl ExactSizeIterator<Item = MappedEntry<T>> + use<'a, T, F> {
        let mut data = std::mem::take(&mut self.data);
        let mut results = Vec::with_capacity(self.docs.len());
        for (len, root_offset) in self.docs.drain(..) {
            let mut doc = QuickwitDoc {
                root_offset,
                data,
                bindings: FieldBindings::default(),
            };
            let meta = before_map(&doc);
            let partition = mapper.quickwit_partition(&doc);
            let data_mut = Arc::get_mut(&mut doc.data).expect("unique BatchData");
            let checkpoint = data_mut.dictionary.len();
            let object = decode_root_object(&data_mut.nodes, root_offset);
            let result = mapper
                .map_tantivy_fields(object, root_offset, &mut data_mut.dictionary, len as u64)
                .inspect_err(|_| data_mut.dictionary.truncate(checkpoint))
                .map(|bindings| (meta, partition, root_offset, bindings));
            data = doc.data;
            results.push((len, result));
        }
        let data_mut = Arc::get_mut(&mut data).expect("unique BatchData");
        data_mut.dictionary.freeze();
        data_mut.nodes.shrink_to_fit();
        if self.scratch.capacity() > MAX_RETAINED_SCRATCH_CAPACITY {
            self.scratch = Vec::new();
            self.simd_buffers = Buffers::default();
            self.tape = Tape(Vec::new());
            self.key_scratch = Vec::new();
        }
        results.into_iter().map(move |(len, result)| {
            (
                len,
                result.map(|(meta, partition, root_offset, bindings)| {
                    let mut doc = QuickwitDoc {
                        root_offset,
                        data: data.clone(),
                        bindings,
                    };
                    mapper.add_field_presence(&mut doc);
                    (meta, partition, doc)
                }),
            )
        })
    }
}

fn push_doc(
    data: &mut Arc<BatchData>,
    docs: &mut Vec<(usize, u32)>,
    original_len: usize,
    write_object: impl FnOnce(&mut ByteDictionary, &mut Vec<u8>) -> serde_json::Result<()>,
) -> serde_json::Result<()> {
    let data = Arc::get_mut(data).expect("unique BatchData");
    let dict_checkpoint = data.dictionary.len();
    let nodes_checkpoint = data.nodes.len();
    let root_offset = u32::try_from(nodes_checkpoint)
        .map_err(|_| Error::custom("document batch exceeds 4 GiB"))?;
    write_object(&mut data.dictionary, &mut data.nodes).inspect_err(|_| {
        data.dictionary.truncate(dict_checkpoint);
        data.nodes.truncate(nodes_checkpoint);
    })?;
    docs.push((original_len, root_offset));
    Ok(())
}

fn write_container(
    out: &mut Vec<u8>,
    tag: u8,
    len: usize,
    write_body: impl FnOnce(&mut Vec<u8>) -> serde_json::Result<()>,
) -> serde_json::Result<()> {
    out.push(tag);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    let body_len_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let body_start = out.len();
    write_body(out)?;
    let body_len = u32::try_from(out.len() - body_start)
        .map_err(|_| Error::custom("document exceeds 4 GiB"))?;
    out[body_len_pos..body_len_pos + 4].copy_from_slice(&body_len.to_le_bytes());
    Ok(())
}

fn write_tape_object<'a>(
    tape: &[Node<'a>],
    cursor: &mut usize,
    dictionary: &mut ByteDictionary,
    out: &mut Vec<u8>,
    key_scratch: &mut Vec<u64>,
) -> serde_json::Result<()> {
    let Some(&Node::Object { len, .. }) = tape.get(*cursor) else {
        return Err(Error::custom("expected an object"));
    };
    *cursor += 1;

    let scratch_base = key_scratch.len();
    for _ in 0..len {
        let Some(&Node::String(key_str)) = tape.get(*cursor) else {
            key_scratch.truncate(scratch_base);
            return Err(Error::custom("expected an object key"));
        };
        *cursor += 1;
        let val_idx = *cursor;
        let Some(val_node) = tape.get(val_idx) else {
            key_scratch.truncate(scratch_base);
            return Err(Error::custom("unexpected end of JSON tape"));
        };
        let key_id = dictionary.intern_str(key_str).map_err(Error::custom)?;
        // Pack `(key_id, val_idx)` so sorting is a single integer sort and duplicate keys end up
        // in ascending `val_idx` order (last occurrence wins).
        key_scratch.push(((key_id as u64) << 32) | (val_idx as u64));
        *cursor += match val_node {
            Node::Object { count, .. } | Node::Array { count, .. } => *count + 1,
            Node::Static(_) | Node::String(_) => 1,
        };
    }

    let entries = &mut key_scratch[scratch_base..];
    if entries.len() > 1 {
        entries.sort_unstable();
    }
    let mut unique_len = 0;
    for read_idx in 0..entries.len() {
        if read_idx + 1 < entries.len()
            && (entries[read_idx] >> 32) == (entries[read_idx + 1] >> 32)
        {
            continue;
        }
        entries[unique_len] = entries[read_idx];
        unique_len += 1;
    }
    key_scratch.truncate(scratch_base + unique_len);

    let res = write_container(out, TAG_OBJECT, unique_len, |out| {
        for idx in scratch_base..scratch_base + unique_len {
            let packed = key_scratch[idx];
            out.extend_from_slice(&((packed >> 32) as u32).to_le_bytes());
            let mut val_cursor = packed as u32 as usize;
            write_tape_value(tape, &mut val_cursor, dictionary, out, key_scratch)?;
        }
        Ok(())
    });
    key_scratch.truncate(scratch_base);
    res
}

fn write_tape_value<'a>(
    tape: &[Node<'a>],
    cursor: &mut usize,
    dictionary: &mut ByteDictionary,
    out: &mut Vec<u8>,
    key_scratch: &mut Vec<u64>,
) -> serde_json::Result<()> {
    let Some(&node) = tape.get(*cursor) else {
        return Err(Error::custom("unexpected end of JSON tape"));
    };
    match node {
        Node::String(text) => {
            *cursor += 1;
            let id = dictionary.intern_value_str(text).map_err(Error::custom)?;
            out.push(TAG_STRING);
            out.extend_from_slice(&id.to_le_bytes());
            Ok(())
        }
        Node::Static(static_node) => {
            *cursor += 1;
            match static_node {
                StaticNode::Null => out.push(TAG_NULL),
                StaticNode::Bool(false) => out.push(TAG_FALSE),
                StaticNode::Bool(true) => out.push(TAG_TRUE),
                StaticNode::I64(val) => {
                    out.push(TAG_I64);
                    out.extend_from_slice(&val.to_le_bytes());
                }
                StaticNode::U64(val) => {
                    out.push(TAG_U64);
                    out.extend_from_slice(&val.to_le_bytes());
                }
                StaticNode::F64(val) => {
                    out.push(TAG_F64);
                    out.extend_from_slice(&val.to_le_bytes());
                }
            }
            Ok(())
        }
        Node::Array { len, .. } => {
            *cursor += 1;
            write_container(out, TAG_ARRAY, len, |out| {
                for _ in 0..len {
                    write_tape_value(tape, cursor, dictionary, out, key_scratch)?;
                }
                Ok(())
            })
        }
        Node::Object { .. } => write_tape_object(tape, cursor, dictionary, out, key_scratch),
    }
}
