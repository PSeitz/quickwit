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

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use fail::fail_point;
use itertools::Itertools;
use quickwit_actors::{Actor, ActorContext};
use quickwit_common::temp_dir::TempDirectory;
use quickwit_directories::write_hotcache;
use quickwit_doc_mapper::NamedField;
use quickwit_doc_mapper::tag_pruning::append_to_tag_set;
use quickwit_proto::search::{ListFieldsEntry, ListFieldsMetadata, ListFieldsType};
use tantivy::index::FieldMetadata;
use tantivy::schema::{FieldType, Type};
use tantivy::{IndexMeta, InvertedIndexReader, ReloadPolicy};
use tracing::{debug, warn};

/// Maximum distinct values allowed for a tag field within a split.
const MAX_VALUES_PER_TAG_FIELD: usize = if cfg!(any(test, feature = "testsuite")) {
    6
} else {
    1000
};

use crate::models::{IndexedSplit, PackagedSplit};

fn list_split_files(
    index_meta: &IndexMeta,
    scratch_directory: &TempDirectory,
) -> io::Result<Vec<PathBuf>> {
    let mut split_files = vec![scratch_directory.path().join("meta.json")];

    for relative_path in index_meta.list_segment_files() {
        let filepath = scratch_directory.path().join(relative_path);
        // Tantivy lists candidate component paths, including optional files
        // that may not exist.
        if filepath.try_exists()? {
            split_files.push(filepath);
        }
    }
    split_files.sort();
    Ok(split_files)
}

fn build_hotcache<W: io::Write>(split_path: &Path, out: &mut W) -> anyhow::Result<()> {
    let mmap_directory = tantivy::directory::MmapDirectory::open(split_path)?;
    write_hotcache(mmap_directory, out)?;
    Ok(())
}

/// Attempts to exhaustively extract the list of terms in a
/// field term dictionary.
///
/// returns None if:
/// - the number of terms exceed MAX_VALUES_PER_TAG_FIELD
/// - some of the terms are not value utf8.
/// - an error occurs.
///
/// Returns None may hurt split pruning and affects performance,
/// but it does not affect Quickwit's result validity.
fn try_extract_terms(
    named_field: &NamedField,
    inv_indexes: &[Arc<InvertedIndexReader>],
    max_terms: usize,
) -> anyhow::Result<Vec<String>> {
    let num_terms = inv_indexes
        .iter()
        .map(|inv_index| inv_index.terms().num_terms())
        .sum::<usize>();
    if num_terms > max_terms {
        bail!(
            "number of unique terms for tag field {} > {}",
            named_field.name,
            max_terms
        );
    }
    let mut terms = Vec::with_capacity(num_terms);
    for inv_index in inv_indexes {
        let mut terms_streamer = inv_index.terms().stream()?;
        while let Some((term_data, _)) = terms_streamer.next() {
            let term = match named_field.field_type {
                FieldType::U64(_) => u64_from_term_data(term_data)?.to_string(),
                FieldType::I64(_) => {
                    tantivy::u64_to_i64(u64_from_term_data(term_data)?).to_string()
                }
                FieldType::F64(_) => {
                    tantivy::u64_to_f64(u64_from_term_data(term_data)?).to_string()
                }
                FieldType::Bool(_) => match u64_from_term_data(term_data)? {
                    0 => false,
                    1 => true,
                    _ => bail!("invalid boolean value"),
                }
                .to_string(),
                FieldType::Bytes(_) => {
                    bail!("tags collection is not allowed on `bytes` fields")
                }
                _ => std::str::from_utf8(term_data)?.to_string(),
            };
            terms.push(term);
        }
    }
    Ok(terms)
}

/// Collects tags, field metadata, split files, and hotcache for a finalized single-segment split.
pub(super) fn package_split<A: Actor>(
    split: IndexedSplit,
    tag_fields: &[NamedField],
    ctx: &ActorContext<A>,
) -> anyhow::Result<PackagedSplit> {
    fail_point!("packager:before");
    let index_meta = split.index.load_metas()?;
    assert_eq!(index_meta.segments.len(), 1);
    debug!(split_id = %split.split_id(), "create-packaged-split");
    let split_files = list_split_files(&index_meta, &split.split_scratch_directory)?;

    // Extracts tag values from inverted indexes only when a field cardinality is less
    // than `MAX_VALUES_PER_TAG_FIELD`.
    debug!(split_id = %split.split_id(), tag_fields =? tag_fields, "extract-tags-values");
    let index_reader = split
        .index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;

    let fields_metadata = split.index.fields_metadata()?;

    let mut tags = BTreeSet::default();
    for named_field in tag_fields {
        let inverted_indexes = index_reader
            .searcher()
            .segment_readers()
            .iter()
            .map(|segment| segment.inverted_index(named_field.field))
            .collect::<Result<Vec<_>, _>>()?;

        match try_extract_terms(named_field, &inverted_indexes, MAX_VALUES_PER_TAG_FIELD) {
            Ok(terms) => {
                append_to_tag_set(&named_field.name, &terms, &mut tags);
            }
            Err(tag_extraction_error) => {
                warn!(err=?tag_extraction_error,  "no field values will be registered in the split metadata");
            }
        }
    }

    ctx.record_progress();

    debug!(split_id = %split.split_id(), "build-hotcache");
    let mut hotcache_bytes = Vec::new();
    build_hotcache(split.split_scratch_directory.path(), &mut hotcache_bytes)?;
    ctx.record_progress();

    let serialized_split_fields = serialize_fields_metadata(&fields_metadata);

    let packaged_split = PackagedSplit {
        serialized_split_fields,
        split_attrs: split.split_attrs,
        split_scratch_directory: split.split_scratch_directory,
        tags,
        split_files,
        hotcache_bytes,
    };
    fail_point!("packager:after");
    Ok(packaged_split)
}

/// Serializes the fields metadata from a split sorted by (name, type).
fn serialize_fields_metadata(fields_metadata: &[FieldMetadata]) -> Vec<u8> {
    let entries = fields_metadata
        .iter()
        .map(field_metadata_to_list_fields_entry)
        .sorted_unstable_by(|left, right| left.cmp_by_name_and_type(right))
        .collect::<Vec<_>>();

    ListFieldsMetadata { entries }.serialize()
}

fn field_metadata_to_list_fields_entry(field_metadata: &FieldMetadata) -> ListFieldsEntry {
    ListFieldsEntry {
        field_name: field_metadata.field_name.to_string(),
        field_type: tantivy_type_to_list_field_type(field_metadata.typ) as i32,
        searchable: field_metadata.is_indexed(),
        aggregatable: field_metadata.is_fast(),
        index_ids: Vec::new(),
        non_searchable_index_ids: Vec::new(),
        non_aggregatable_index_ids: Vec::new(),
        // Split counts are populated by the search leaf when it reads this metadata.
        num_splits: 0,
    }
}

fn tantivy_type_to_list_field_type(typ: Type) -> ListFieldsType {
    match typ {
        Type::Bool => ListFieldsType::Bool,
        Type::Bytes => ListFieldsType::Bytes,
        Type::Date => ListFieldsType::Date,
        Type::F64 => ListFieldsType::F64,
        Type::Facet => ListFieldsType::Facet,
        Type::I64 => ListFieldsType::I64,
        Type::IpAddr => ListFieldsType::IpAddr,
        Type::Json => ListFieldsType::Json,
        Type::Str => ListFieldsType::Str,
        Type::U64 => ListFieldsType::U64,
        // Packaged fields originate from Quickwit mappings, which cannot define custom types.
        Type::Custom => unimplemented!("custom fields are not supported in Quickwit"),
    }
}

/// Reads u64 from stored term data.
fn u64_from_term_data(data: &[u8]) -> anyhow::Result<u64> {
    let u64_bytes: [u8; 8] = data[0..8]
        .try_into()
        .context("could not interpret term bytes as u64")?;
    Ok(u64::from_be_bytes(u64_bytes))
}

#[cfg(test)]
#[path = "split_packaging_tests.rs"]
mod tests;
