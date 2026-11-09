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

use std::fmt;
use std::ops::RangeInclusive;
use std::path::Path;

use anyhow::Context;
use quickwit_common::io::IoControls;
use quickwit_common::metrics::index_label;
use quickwit_common::temp_dir::TempDirectory;
use quickwit_config::IndexingSettings;
use quickwit_doc_mapper::DocMapper;
use quickwit_metastore::checkpoint::IndexCheckpointDelta;
use quickwit_metrics::{GaugeGuard, label_values};
use quickwit_proto::indexing::IndexingPipelineId;
use quickwit_proto::types::{DocMappingUid, IndexUid, SplitId};
use quickwit_query::get_quickwit_fastfield_normalizer_manager;
use tantivy::directory::{MmapDirectory, RamDirectory};
use tantivy::store::{Compressor, ZstdCompressor};
use tantivy::{DateTime, DocId, IndexBuilder, IndexSettings};
use tracing::{Span, error, instrument};

use crate::controlled_directory::ControlledDirectory;
use crate::docs_clustering::DocIdClusterer;
use crate::merge_policy::MergeTask;
use crate::metrics::INDEX_SOURCE;
use crate::models::{ProcessedDoc, PublishLock, SplitAttrs};

pub struct IndexedSplitBuilder {
    pub split_attrs: SplitAttrs,
    pub index_writer: tantivy::SingleSegmentIndexWriter,
    pub split_scratch_directory: TempDirectory,
    pub controlled_directory: ControlledDirectory,
    pub doc_id_clusterer_opt: Option<DocIdClusterer>,
    ram_directory_opt: Option<RamDirectory>,
}

pub struct IndexedSplit {
    pub split_attrs: SplitAttrs,
    pub index: tantivy::Index,
    pub split_scratch_directory: TempDirectory,
    pub controlled_directory: ControlledDirectory,
}

impl IndexedSplit {
    pub fn split_id(&self) -> &SplitId {
        &self.split_attrs.split_id
    }
}

impl fmt::Debug for IndexedSplit {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter
            .debug_struct("IndexedSplit")
            .field("split_id", &self.split_attrs.split_id)
            .field("dir", &self.split_scratch_directory.path())
            .field("num_docs", &self.split_attrs.num_docs)
            .finish()
    }
}

impl fmt::Debug for IndexedSplitBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter
            .debug_struct("IndexedSplitBuilder")
            .field("split_id", &self.split_attrs.split_id)
            .field("dir", &self.split_scratch_directory.path())
            .field("num_docs", &self.split_attrs.num_docs)
            .field("doc_id_clusterer_opt", &self.doc_id_clusterer_opt.is_some())
            .finish()
    }
}

impl IndexedSplitBuilder {
    /// Creates a split using the mapper's schema, tokenizers, and mapping UID.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_doc_mapper(
        pipeline_id: IndexingPipelineId,
        partition_id: u64,
        last_delete_opstamp: u64,
        doc_mapper: &DocMapper,
        indexing_settings: &IndexingSettings,
        scratch_directory: TempDirectory,
        io_controls: IoControls,
        clustering_enabled: bool,
    ) -> anyhow::Result<Self> {
        let docstore_compression = Compressor::Zstd(ZstdCompressor {
            compression_level: Some(indexing_settings.docstore_compression_level),
        });
        let index_settings = IndexSettings {
            sort_by_field: None,
            docstore_blocksize: indexing_settings.docstore_blocksize,
            docstore_compression,
            docstore_compress_dedicated_thread: true,
            // The clusterer supplies the mapping when the split is finalized.
            manual_doc_id_mapping: clustering_enabled,
        };
        let index_builder = IndexBuilder::new()
            .settings(index_settings)
            .schema(doc_mapper.schema())
            .tokenizers(doc_mapper.tokenizer_manager().tantivy_manager().clone())
            .fast_field_tokenizers(
                get_quickwit_fastfield_normalizer_manager()
                    .tantivy_manager()
                    .clone(),
            );
        Self::new_in_dir(
            pipeline_id,
            partition_id,
            last_delete_opstamp,
            doc_mapper.doc_mapping_uid(),
            scratch_directory,
            index_builder,
            io_controls,
            clustering_enabled.then(DocIdClusterer::default),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_in_dir(
        pipeline_id: IndexingPipelineId,
        partition_id: u64,
        last_delete_opstamp: u64,
        doc_mapping_uid: DocMappingUid,
        scratch_directory: TempDirectory,
        index_builder: IndexBuilder,
        io_controls: IoControls,
        doc_id_clusterer_opt: Option<DocIdClusterer>,
    ) -> anyhow::Result<Self> {
        // We avoid intermediary merge, and instead merge all segments in the packager.
        // The benefit is that we don't have to wait for potentially existing merges,
        // and avoid possible race conditions.
        let split_id = SplitId::new();
        let split_scratch_directory_prefix = format!("split-{split_id}-");
        let split_scratch_directory =
            scratch_directory.named_temp_child(&split_scratch_directory_prefix)?;
        let use_ram_directory =
            quickwit_common::get_bool_from_env_cached!("QW_ENABLE_IN_MEMORY_INDEXING", false);
        let ram_directory_opt = use_ram_directory.then(RamDirectory::default);
        let mmap_directory = MmapDirectory::open(split_scratch_directory.path())?;
        let controlled_directory = ControlledDirectory::new(Box::new(mmap_directory), io_controls);
        let indexing_directory: Box<dyn tantivy::Directory> =
            if let Some(ram_directory) = &ram_directory_opt {
                Box::new(ram_directory.clone())
            } else {
                Box::new(controlled_directory.clone())
            };

        let index_writer =
            index_builder.single_segment_index_writer(indexing_directory, 15_000_000)?;
        Ok(Self {
            split_attrs: SplitAttrs {
                node_id: pipeline_id.node_id,
                index_uid: pipeline_id.index_uid,
                source_id: pipeline_id.source_id,
                doc_mapping_uid,
                partition_id,
                split_id,
                num_docs: 0,
                replaced_split_ids: Vec::new(),
                uncompressed_docs_size_in_bytes: 0,
                time_range: None,
                delete_opstamp: last_delete_opstamp,
                num_merge_ops: 0,
            },
            index_writer,
            doc_id_clusterer_opt,
            split_scratch_directory,
            controlled_directory,
            ram_directory_opt,
        })
    }

    /// Adds a document and updates the split's metadata and clustering.
    /// The document must use this split's schema. The caller handles partition routing.
    /// Discard the builder on error: its metadata is updated before writing the document.
    pub(crate) fn add_document(&mut self, processed_doc: ProcessedDoc) -> anyhow::Result<()> {
        let ProcessedDoc {
            doc,
            fingerprint_opt,
            timestamp_opt,
            num_bytes,
            ..
        } = processed_doc;
        self.split_attrs.uncompressed_docs_size_in_bytes += num_bytes as u64;
        // Tantivy doc IDs are local to the split and continue across processed-doc batches.
        let split_doc_id = self.split_attrs.num_docs as DocId;
        self.split_attrs.num_docs += 1;
        if let Some(doc_id_clusterer) = self.doc_id_clusterer_opt.as_mut() {
            doc_id_clusterer.push(fingerprint_opt, split_doc_id);
        }
        if let Some(timestamp) = timestamp_opt {
            record_timestamp(timestamp, &mut self.split_attrs.time_range);
        }
        self.index_writer
            .add_document(doc)
            .context("failed to add document")?;
        Ok(())
    }

    #[instrument(name="serialize_split",
        skip_all,
        fields(
            node_id=%self.split_attrs.node_id,
            index_uid=%self.split_attrs.index_uid,
            source_id=%self.split_attrs.source_id,
            split_id=%self.split_attrs.split_id,
            partition_id=%self.split_attrs.partition_id,
            num_docs=%self.split_attrs.num_docs,
            uncompressed_docs_size_in_bytes=%self.split_attrs.uncompressed_docs_size_in_bytes,
            delete_opstamp=%self.split_attrs.delete_opstamp,
            num_merge_ops=%self.split_attrs.num_merge_ops,
        )
    )]
    pub fn finalize(self) -> anyhow::Result<IndexedSplit> {
        let split_attrs = self.split_attrs;
        let index = if let Some(doc_id_clusterer) = self.doc_id_clusterer_opt {
            // Update metrics for document clustering.
            let index_label = index_label(&split_attrs.index_uid.index_id);
            let labels = label_values!(
                INDEX_SOURCE => index_label.to_string(),
                split_attrs.source_id.to_string()
            );
            doc_id_clusterer.observe_cluster_group_sizes(labels);

            // Finalize the index with the doc id mapping.
            let doc_id_mapping = doc_id_clusterer
                .into_doc_id_mapping()
                .inspect_err(|error| {
                    error!(?error, "failed to create doc id mapping");
                })?;
            self.index_writer
                .finalize_with_doc_id_mapping(&doc_id_mapping)?
        } else {
            self.index_writer.finalize()?
        };
        let index = if let Some(ram_directory) = self.ram_directory_opt {
            ram_directory.persist(&self.controlled_directory)?;
            // Persist copies files but leaves RAM intact. Return a disk-backed index so packaging
            // does not retain the RAM files. The builder's RAM handle drops at this block's end;
            // the original RAM-backed `index` (shadowed by this binding) drops when
            // finalize returns.
            tantivy::Index::open(self.controlled_directory.clone())?
        } else {
            index
        };
        Ok(IndexedSplit {
            split_attrs,
            index,
            split_scratch_directory: self.split_scratch_directory,
            controlled_directory: self.controlled_directory,
        })
    }

    pub fn path(&self) -> &Path {
        self.split_scratch_directory.path()
    }

    pub fn mem_usage(&self) -> usize {
        self.index_writer.mem_usage()
            + self
                .ram_directory_opt
                .as_ref()
                .map(RamDirectory::total_mem_usage)
                .unwrap_or(0)
    }

    pub fn split_id(&self) -> &SplitId {
        &self.split_attrs.split_id
    }
}

fn record_timestamp(timestamp: DateTime, time_range: &mut Option<RangeInclusive<DateTime>>) {
    let new_timestamp_range = match time_range {
        Some(range) => timestamp.min(*range.start())..=timestamp.max(*range.end()),
        None => timestamp..=timestamp,
    };
    *time_range = Some(new_timestamp_range);
}

#[derive(Debug)]
pub struct IndexedSplitBatch {
    pub splits: Vec<IndexedSplit>,
    pub checkpoint_delta_opt: Option<IndexCheckpointDelta>,
    pub publish_lock: PublishLock,
    /// A [`MergeTask`] tracked by either the `MergePlanner` or the `DeleteTaskPlanner`
    /// in the `MergePipeline` or `DeleteTaskPipeline`.
    /// See planners docs to understand the usage.
    /// If `None`, the split batch was built in the `IndexingPipeline`.
    pub merge_task_opt: Option<MergeTask>,
    pub batch_parent_span: Span,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitTrigger {
    Drained,
    ForceCommit,
    MemoryLimit,
    NoMoreDocs,
    NumDocsLimit,
    Timeout,
}

#[derive(Debug)]
pub struct IndexedSplitBatchBuilder {
    pub splits: Vec<IndexedSplitBuilder>,
    pub checkpoint_delta_opt: Option<IndexCheckpointDelta>,
    pub publish_lock: PublishLock,
    pub commit_trigger: CommitTrigger,
    pub batch_parent_span: Span,
    pub memory_usage: GaugeGuard,
    pub _split_builders_guard: GaugeGuard,
}

/// Sends notifications to the Publisher that the last batch of splits was empty.
#[derive(Debug)]
pub struct EmptySplit {
    pub index_uid: IndexUid,
    pub checkpoint_delta: IndexCheckpointDelta,
    pub publish_lock: PublishLock,
    pub batch_parent_span: Span,
}

#[cfg(test)]
mod tests {
    use quickwit_proto::types::NodeId;
    use tantivy::schema::{STORED, Schema};
    use tantivy::{Directory, DocAddress, TantivyDocument, doc};

    use super::*;

    #[test]
    fn test_record_timestamp() {
        let mut time_range = None;
        record_timestamp(DateTime::from_timestamp_secs(1628664679), &mut time_range);
        assert_eq!(
            time_range,
            Some(
                DateTime::from_timestamp_secs(1628664679)
                    ..=DateTime::from_timestamp_secs(1628664679)
            )
        );
        record_timestamp(DateTime::from_timestamp_secs(1628664112), &mut time_range);
        assert_eq!(
            time_range,
            Some(
                DateTime::from_timestamp_secs(1628664112)
                    ..=DateTime::from_timestamp_secs(1628664679)
            )
        );
        record_timestamp(DateTime::from_timestamp_secs(1628665112), &mut time_range);
        assert_eq!(
            time_range,
            Some(
                DateTime::from_timestamp_secs(1628664112)
                    ..=DateTime::from_timestamp_secs(1628665112)
            )
        )
    }

    #[test]
    fn test_finalize_ram_split_uses_disk() -> anyhow::Result<()> {
        let mut schema = Schema::builder();
        let id = schema.add_u64_field("id", STORED);
        let ram_directory = RamDirectory::default();
        let mut index_writer = IndexBuilder::new()
            .schema(schema.build())
            .single_segment_index_writer(ram_directory.clone(), 15_000_000)?;
        index_writer.add_document(doc!(id => 42u64))?;
        let split_scratch_directory = TempDirectory::for_test();
        let controlled_directory = ControlledDirectory::new(
            Box::new(MmapDirectory::open(split_scratch_directory.path())?),
            IoControls::default(),
        );
        // Select RAM directly: the production environment flag is cached process-wide.
        let builder = IndexedSplitBuilder {
            split_attrs: SplitAttrs {
                node_id: NodeId::from_str("test-node"),
                index_uid: IndexUid::new_with_random_ulid("test-index"),
                source_id: "test-source".to_string(),
                doc_mapping_uid: DocMappingUid::default(),
                split_id: SplitId::new(),
                partition_id: 0,
                num_docs: 1,
                uncompressed_docs_size_in_bytes: 0,
                time_range: None,
                replaced_split_ids: Vec::new(),
                delete_opstamp: 0,
                num_merge_ops: 0,
            },
            index_writer,
            split_scratch_directory,
            controlled_directory,
            doc_id_clusterer_opt: None,
            ram_directory_opt: Some(ram_directory.clone()),
        };
        let split = builder.finalize()?;
        // A file added only after persistence must not be visible through the returned index.
        let marker = Path::new("ram-only");
        ram_directory.atomic_write(marker, b"not persisted")?;
        assert!(!split.index.directory().exists(marker)?);
        drop(ram_directory);

        let reader = split.index.reader()?;
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 1);
        let id = split.index.schema().get_field("id")?;
        let document: TantivyDocument = searcher.doc(DocAddress::new(0, 0))?;
        assert_eq!(document, doc!(id => 42u64));
        Ok(())
    }
}
