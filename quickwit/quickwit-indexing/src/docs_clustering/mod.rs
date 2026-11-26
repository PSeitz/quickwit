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

//! Up-front document clustering for fresh splits.
//!
//! [`DocClusterer`] retains mapped [`QuickwitDoc`] instances in nested groups during split
//! collection and drains them into Tantivy in cluster order at finalization, avoiding post-hoc
//! document ID remapping and docstore rewrites.
//!
//! Each entry in [`DocsClusteringConfig::group_by`] defines one grouping level from outermost to
//! innermost:
//! - [`ClusteringKey::Field`] groups by string values at a dot-separated JSON path on
//!   [`QuickwitDoc`]. Arrays use their first value. Missing and non-string values share an ignored
//!   bucket, which precedes string groups in ascending lexicographic order.
//! - [`ClusteringKey::Tokenized`] groups by the sequence of [`tokenizer::TokenType`] tags for the
//!   first `max_tokens` tokens (default 50) of the string value at a dot-separated JSON path,
//!   ordering values with identical token-type templates together and adjacent to shared prefixes.
//!   Missing and non-string values share an ignored bucket before tokenized groups.
//! - [`ClusteringKey::Structure`] groups by an order-independent 64-bit hash of the document's
//!   unique JSON leaf paths (values, scalar types, and key order are ignored; arrays count as
//!   leaves; empty objects contribute no leaves), ordered by ascending hash.
//!
//! Documents within each leaf group retain arrival order.

#[cfg(test)]
mod tests;
mod tokenizer;

use std::hash::{Hash, Hasher};
use std::mem::size_of;

use quickwit_config::{ClusteringKey, DocsClusteringConfig};
use quickwit_doc_mapper::{
    ByteDictionary, QuickwitDoc, QuickwitLeafValue, QuickwitObject, QuickwitValue,
};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

const DEFAULT_MAX_GROUPING_TOKENS: usize = 50;

enum Grouping {
    Field {
        path: Vec<String>,
    },
    Tokenized {
        path: Vec<String>,
        max_tokens: usize,
    },
    Structure,
}

#[derive(Debug, Eq, PartialEq, Hash)]
enum GroupKey<'a> {
    Ignored,
    Str(&'a str),
    Tokenized { text: &'a str, max_tokens: usize },
    Structure(u64),
}

fn extract_str<'a>(
    value: QuickwitValue<'a>,
    path: &[String],
    dict: &'a ByteDictionary,
) -> Option<&'a str> {
    match value {
        QuickwitValue::Array(array) => array
            .first()
            .and_then(|value| extract_str(value, path, dict)),
        QuickwitValue::Object(object) => {
            let (segment, rest) = path.split_first()?;
            object
                .get(segment, dict)
                .and_then(|value| extract_str(value, rest, dict))
        }
        QuickwitValue::Leaf(_) if !path.is_empty() => None,
        QuickwitValue::Leaf(QuickwitLeafValue::String(id)) => Some(dict.get_str(id)),
        QuickwitValue::Leaf(_) => None,
    }
}

struct GroupNode<T> {
    // Each level has one key kind: UTF-8 strings, token-type byte sequences, or big-endian
    // structure hashes.
    children: FxHashMap<Vec<u8>, GroupNode<T>>,
    // Keep ignored values separate so the empty string / empty token pattern still has its own
    // group.
    ignored: Option<Box<GroupNode<T>>>,
    documents: Vec<T>,
}

impl<T> Default for GroupNode<T> {
    fn default() -> Self {
        Self {
            children: FxHashMap::default(),
            ignored: None,
            documents: Vec::new(),
        }
    }
}

impl<T> GroupNode<T> {
    // Locate the leaf before moving the document whose keys we borrow.
    fn leaf<'a>(
        &mut self,
        mut group_keys: impl Iterator<Item = GroupKey<'a>>,
        token_buf: &mut Vec<u8>,
        mem_usage: &mut usize,
    ) -> &mut Vec<T> {
        let Some(key) = group_keys.next() else {
            return &mut self.documents;
        };
        let hash_bytes;
        let key_bytes = match &key {
            GroupKey::Str(text) => text.as_bytes(),
            GroupKey::Tokenized { text, max_tokens } => {
                token_buf.clear();
                token_buf.extend(
                    tokenizer::tokenize(text)
                        .take(*max_tokens)
                        .map(|span| span.token_type as u8),
                );
                token_buf.as_slice()
            }
            GroupKey::Structure(hash) => {
                hash_bytes = hash.to_be_bytes();
                &hash_bytes
            }
            GroupKey::Ignored => {
                return self
                    .ignored
                    .get_or_insert_with(|| {
                        *mem_usage += size_of::<Self>();
                        Box::default()
                    })
                    .leaf(group_keys, token_buf, mem_usage);
            }
        };
        if !self.children.contains_key(key_bytes) {
            let capacity = self.children.capacity();
            let owned_key = key_bytes.to_vec();
            *mem_usage += owned_key.capacity();
            self.children.insert(owned_key, Self::default());
            // Include empty buckets and the trailing control-byte group.
            let bucket_growth =
                (self.children.capacity() * 8).div_ceil(7) - (capacity * 8).div_ceil(7);
            *mem_usage += bucket_growth * (size_of::<(Vec<u8>, Self)>() + 1)
                + if capacity == 0 { 16 } else { 0 };
        }
        self.children
            .get_mut(key_bytes)
            .unwrap()
            .leaf(group_keys, token_buf, mem_usage)
    }

    fn collect_group_sizes(&self, sizes: &mut Vec<usize>) {
        if !self.documents.is_empty() {
            sizes.push(self.documents.len());
        }
        if let Some(ignored) = &self.ignored {
            ignored.collect_group_sizes(sizes);
        }
        let mut children: Vec<_> = self.children.iter().collect();
        children.sort_unstable_by_key(|(left, _)| *left);
        for (_, child) in children {
            child.collect_group_sizes(sizes);
        }
    }

    fn drain<E>(self, visit: &mut impl FnMut(T) -> Result<(), E>) -> Result<(), E> {
        for document in self.documents {
            visit(document)?;
        }
        // Collecting drops the hash table before visiting children.
        let mut children: Vec<_> = self.children.into_iter().collect();
        children.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        if let Some(ignored) = self.ignored {
            ignored.drain(visit)?;
        }
        for (_, child) in children {
            child.drain(visit)?;
        }
        Ok(())
    }
}

fn split_path(path: &str) -> Vec<String> {
    path.split('.').map(str::to_string).collect()
}

fn build_grouping(keys: &[ClusteringKey]) -> Vec<Grouping> {
    keys.iter()
        .map(|key| match key {
            ClusteringKey::Field(path) => Grouping::Field {
                path: split_path(path),
            },
            ClusteringKey::Tokenized { path, max_tokens } => Grouping::Tokenized {
                path: split_path(path),
                max_tokens: max_tokens.unwrap_or(DEFAULT_MAX_GROUPING_TOKENS),
            },
            ClusteringKey::Structure => Grouping::Structure,
        })
        .collect()
}

#[inline]
fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn structure_hash(doc: &QuickwitDoc) -> u64 {
    fn hash_object(
        object: QuickwitObject<'_>,
        dict: &ByteDictionary,
        path: FxHasher,
        acc: &mut u64,
    ) {
        for (key, value) in object.iter(dict) {
            let mut child = path.clone();
            // `str::hash` writes a `0xff` terminator so nested path boundaries remain distinct.
            key.hash(&mut child);
            match value {
                QuickwitValue::Object(nested) => hash_object(nested, dict, child, acc),
                _ => *acc = acc.wrapping_add(mix64(child.finish())),
            }
        }
    }

    let mut acc = 0u64;
    hash_object(
        doc.as_object(),
        doc.dictionary(),
        FxHasher::default(),
        &mut acc,
    );
    acc
}

fn group_keys<'a>(
    grouping: &'a [Grouping],
    doc: &'a QuickwitDoc,
) -> impl Iterator<Item = GroupKey<'a>> + 'a {
    let mut structure = None;
    let root = QuickwitValue::Object(doc.as_object());
    let dict = doc.dictionary();
    grouping.iter().map(move |key| match key {
        Grouping::Field { path } => extract_str(root, path, dict)
            .map(GroupKey::Str)
            .unwrap_or(GroupKey::Ignored),
        Grouping::Tokenized { path, max_tokens } => extract_str(root, path, dict)
            .map(|text| GroupKey::Tokenized {
                text,
                max_tokens: *max_tokens,
            })
            .unwrap_or(GroupKey::Ignored),
        Grouping::Structure => {
            GroupKey::Structure(*structure.get_or_insert_with(|| structure_hash(doc)))
        }
    })
}

/// Buffers mapped [`QuickwitDoc`] instances in cluster order until split finalization.
pub struct DocClusterer {
    grouping: Vec<Grouping>,
    root: GroupNode<QuickwitDoc>,
    token_buf: Vec<u8>,
    seen_batches: FxHashSet<usize>,
    mem_usage: usize,
}

impl DocClusterer {
    /// Validates `config` and initializes a clusterer.
    pub fn new(config: &DocsClusteringConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let grouping = build_grouping(&config.group_by);
        let mem_usage = size_of::<Self>()
            + grouping.capacity() * size_of::<Grouping>()
            + grouping
                .iter()
                .map(|key| match key {
                    Grouping::Field { path } | Grouping::Tokenized { path, .. } => {
                        path.capacity() * size_of::<String>()
                            + path.iter().map(String::capacity).sum::<usize>()
                    }
                    Grouping::Structure => 0,
                })
                .sum::<usize>();
        Ok(Self {
            grouping,
            root: GroupNode::default(),
            token_buf: Vec::new(),
            seen_batches: FxHashSet::default(),
            mem_usage,
        })
    }

    /// Retains `document` in its cluster group and updates the retained memory estimate.
    pub fn push(&mut self, document: QuickwitDoc) {
        self.mem_usage += document.bindings_mem_usage();
        let (batch_id, batch_mem_usage) = document.batch_data_id_and_mem_usage();
        if self.seen_batches.insert(batch_id) {
            self.mem_usage += batch_mem_usage;
        }
        let leaf = self.root.leaf(
            group_keys(&self.grouping, &document),
            &mut self.token_buf,
            &mut self.mem_usage,
        );
        let capacity = leaf.capacity();
        leaf.push(document);
        self.mem_usage += (leaf.capacity() - capacity) * size_of::<QuickwitDoc>();
    }

    /// Returns leaf group counts in drain order.
    pub fn group_sizes(&self) -> Vec<usize> {
        let mut sizes = Vec::new();
        self.root.collect_group_sizes(&mut sizes);
        sizes
    }

    /// Estimated retained memory across buffered documents, shared batch buffers, and grouping
    /// structures.
    pub fn mem_usage(&self) -> usize {
        self.mem_usage + self.token_buf.capacity()
    }

    /// Consumes the clusterer and visits every document in cluster order.
    pub fn drain<E>(self, visit: &mut impl FnMut(QuickwitDoc) -> Result<(), E>) -> Result<(), E> {
        self.root.drain(visit)
    }
}
