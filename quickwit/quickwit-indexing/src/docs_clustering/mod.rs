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
//! - [`ClusteringKey::Field`] groups by string values at a mapped schema or JSON path. Repeated
//!   fields and arrays use their first value. Missing and non-string values share an ignored
//!   bucket, which precedes string groups in ascending lexicographic order. Unmapped dynamic values
//!   require explicit paths such as `_dynamic.service`.
//! - [`ClusteringKey::Structure`] groups by a 64-bit hash of the document's unique mapped leaf
//!   paths (values, scalar types, field order, and repetitions are ignored; arrays count as leaves;
//!   empty objects contribute no leaves), ordered by ascending hash.
//!
//! Documents within each leaf group retain arrival order.

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::mem::size_of;

use anyhow::{Context, bail, ensure};
use quickwit_config::{ClusteringKey, DocsClusteringConfig};
use quickwit_doc_mapper::QuickwitDoc;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use tantivy::json_utils::split_json_path;
use tantivy::schema::document::{ReferenceValue, ReferenceValueLeaf, Value};
use tantivy::schema::{Document, Field, FieldType, Schema};

enum Grouping {
    Field {
        field: Field,
        json_path: Vec<String>,
    },
    Structure,
}

#[derive(Debug, Eq, PartialEq, Hash)]
enum GroupKey<'a> {
    Ignored,
    Str(Cow<'a, str>),
    Structure(u64),
}

impl<'a> GroupKey<'a> {
    fn from_value<V: Value<'a>>(value: V, json_path: &[String]) -> Self {
        match value.as_value() {
            ReferenceValue::Array(mut values) => values
                .next()
                .map(|value| Self::from_value(value, json_path))
                .unwrap_or(Self::Ignored),
            ReferenceValue::Object(mut values) => {
                let Some((segment, rest)) = json_path.split_first() else {
                    return Self::Ignored;
                };
                values
                    .find(|(key, _)| *key == segment)
                    .map(|(_, value)| Self::from_value(value, rest))
                    .unwrap_or(Self::Ignored)
            }
            ReferenceValue::Leaf(_) if !json_path.is_empty() => Self::Ignored,
            ReferenceValue::Leaf(leaf) => match leaf {
                ReferenceValueLeaf::Str(text) => Self::Str(Cow::Borrowed(text)),
                ReferenceValueLeaf::PreTokStr(value) => Self::Str(Cow::Owned(value.text)),
                _ => Self::Ignored,
            },
        }
    }
}

struct GroupNode<T> {
    // Each level has one key kind: UTF-8 strings or big-endian structure hashes.
    children: FxHashMap<Vec<u8>, GroupNode<T>>,
    // Keep ignored values separate so the empty string still has its own group.
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
        mem_usage: &mut usize,
    ) -> &mut Vec<T> {
        let Some(key) = group_keys.next() else {
            return &mut self.documents;
        };
        let hash_bytes;
        let key_bytes = match &key {
            GroupKey::Str(text) => text.as_bytes(),
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
                    .leaf(group_keys, mem_usage);
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
            .leaf(group_keys, mem_usage)
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

fn resolve_grouping(schema: &Schema, keys: &[ClusteringKey]) -> anyhow::Result<Vec<Grouping>> {
    ensure!(!keys.is_empty(), "clustering requires at least one key");
    keys.iter()
        .map(|key| {
            let ClusteringKey::Field(name) = key else {
                return Ok(Grouping::Structure);
            };
            let (field, json_path) = schema
                .find_field(name)
                .with_context(|| format!("clustering field `{name}` not found in schema"))?;
            if !json_path.is_empty()
                && !matches!(
                    schema.get_field_entry(field).field_type(),
                    FieldType::JsonObject(_)
                )
            {
                bail!("clustering path `{name}` is not inside a JSON field");
            }
            Ok(Grouping::Field {
                field,
                json_path: if json_path.is_empty() {
                    Vec::new()
                } else {
                    split_json_path(json_path)
                },
            })
        })
        .collect()
}

fn structure_hash<D: Document>(document: &D) -> u64 {
    fn collect<'a, V: Value<'a>>(value: V, path: FxHasher, leaves: &mut Vec<u64>) {
        match value.as_value() {
            ReferenceValue::Object(values) => {
                for (key, value) in values {
                    let mut child = path.clone();
                    // Hash includes the component boundary, unlike concatenating path text.
                    key.hash(&mut child);
                    collect(value, child, leaves);
                }
            }
            _ => leaves.push(path.finish()),
        }
    }

    let mut leaves = Vec::new();
    for (field, value) in document.iter_fields_and_values() {
        let mut path = FxHasher::default();
        field.hash(&mut path);
        collect(value, path, &mut leaves);
    }
    leaves.sort_unstable();
    leaves.dedup();
    let mut hash = FxHasher::default();
    leaves.hash(&mut hash);
    hash.finish()
}

fn group_keys<'a, D: Document>(
    grouping: &'a [Grouping],
    document: &'a D,
) -> impl Iterator<Item = GroupKey<'a>> + 'a {
    let mut structure = None;
    grouping.iter().map(move |key| match key {
        Grouping::Field { field, json_path } => document
            .iter_fields_and_values()
            .find(|(candidate, _)| candidate == field)
            .map(|(_, value)| GroupKey::from_value(value, json_path))
            .unwrap_or(GroupKey::Ignored),
        Grouping::Structure => {
            GroupKey::Structure(*structure.get_or_insert_with(|| structure_hash(document)))
        }
    })
}

/// Buffers mapped [`QuickwitDoc`] instances in cluster order until split finalization.
pub struct DocClusterer {
    grouping: Vec<Grouping>,
    root: GroupNode<QuickwitDoc>,
    seen_batches: FxHashSet<usize>,
    mem_usage: usize,
}

impl DocClusterer {
    /// Resolves and validates `config` against `schema`.
    pub fn new(schema: &Schema, config: &DocsClusteringConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let grouping = resolve_grouping(schema, &config.group_by)
            .context("invalid clustering schema paths (dynamic fields require `_dynamic.`)")?;
        let mem_usage = size_of::<Self>()
            + grouping.capacity() * size_of::<Grouping>()
            + grouping
                .iter()
                .map(|key| match key {
                    Grouping::Field { json_path, .. } => {
                        json_path.capacity() * size_of::<String>()
                            + json_path.iter().map(String::capacity).sum::<usize>()
                    }
                    Grouping::Structure => 0,
                })
                .sum::<usize>();
        Ok(Self {
            grouping,
            root: GroupNode::default(),
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
        let leaf = self
            .root
            .leaf(group_keys(&self.grouping, &document), &mut self.mem_usage);
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
        self.mem_usage
    }

    /// Consumes the clusterer and visits every document in cluster order.
    pub fn drain<E>(self, visit: &mut impl FnMut(QuickwitDoc) -> Result<(), E>) -> Result<(), E> {
        self.root.drain(visit)
    }
}
