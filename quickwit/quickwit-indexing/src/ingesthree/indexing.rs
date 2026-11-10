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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, ensure};
use bytes::Bytes;
use quickwit_common::io::IoControls;
use quickwit_common::metrics::IN_FLIGHT_INDEX_WRITER;
use quickwit_common::temp_dir::TempDirectory;
use quickwit_config::{
    IndexConfig, IndexingSettings, RetentionPolicy, SourceInputFormat, build_doc_mapper,
};
use quickwit_doc_mapper::DocMapper;
use quickwit_ingest::ingesthree::DocumentFileReader;
use quickwit_metastore::SplitMetadata;
use quickwit_metrics::{Gauge, GaugeGuard};
use quickwit_proto::indexing::IndexingPipelineId;
use quickwit_proto::metastore::DocumentsStagedForUpload;
use quickwit_storage::SplitPayload;

use crate::actors::{
    DocumentConverter, OTHER_PARTITION_ID, create_packaged_split, prepare_split_for_upload,
};
use crate::docs_clustering::Fingerprinter;
use crate::merge_policy::MergePolicy;
use crate::models::{IndexedSplit, IndexedSplitBuilder, PackagedSplit, ProcessedDoc};

/// Keeps packaged files and their upload metadata alive until async upload completes.
pub(super) struct PreparedSplit {
    pub packaged: PackagedSplit,
    pub metadata: SplitMetadata,
    pub payload: SplitPayload,
}

/// A split writer whose memory is counted until serialization finishes.
/// Field order releases the builder before its memory guard on error or cancellation.
pub(super) struct SplitWriter {
    pub builder: IndexedSplitBuilder,
    memory_usage: GaugeGuard,
}

impl SplitWriter {
    fn new(builder: IndexedSplitBuilder, memory_gauge: &Gauge) -> Self {
        let memory_usage = GaugeGuard::new(memory_gauge, builder.mem_usage() as f64);
        Self {
            builder,
            memory_usage,
        }
    }

    fn add_document(&mut self, document: ProcessedDoc) -> anyhow::Result<()> {
        let result = self.builder.add_document(document);
        self.memory_usage
            .increment(self.builder.mem_usage() as f64 - self.memory_usage.delta());
        result
    }

    pub fn finalize(self) -> anyhow::Result<IndexedSplit> {
        let Self {
            builder,
            memory_usage,
        } = self;
        let result = builder.finalize();
        drop(memory_usage);
        result
    }
}

/// Converts documents and accumulates splits until their document or memory target is reached.
pub(super) struct SplitBuilders {
    converter: DocumentConverter,
    mapper: Arc<DocMapper>,
    pipeline_id: IndexingPipelineId,
    settings: IndexingSettings,
    delete_opstamp: u64,
    directory: TempDirectory,
    pub io_controls: IoControls,
    clustering_enabled: bool,
    splits: HashMap<u64, SplitWriter>,
    overflow: Option<SplitWriter>,
    writer_memory: Gauge,
}

impl SplitBuilders {
    pub fn new(
        config: IndexConfig,
        pipeline_id: IndexingPipelineId,
        delete_opstamp: u64,
        directory: TempDirectory,
        io_controls: IoControls,
        fingerprinter: Option<Fingerprinter>,
    ) -> anyhow::Result<Self> {
        let mapper = build_doc_mapper(&config.doc_mapping, &config.search_settings)?;
        // Ingesthree files contain JSON documents. Source decoding/transforms belong before intake.
        let converter = DocumentConverter::try_new(
            pipeline_id.index_uid.index_id.clone(),
            pipeline_id.source_id.clone(),
            mapper.clone(),
            None,
            SourceInputFormat::Json,
            fingerprinter.clone(),
        )?;
        Ok(Self {
            converter,
            mapper,
            pipeline_id,
            settings: config.indexing_settings,
            delete_opstamp,
            directory,
            io_controls,
            clustering_enabled: fingerprinter.is_some(),
            splits: HashMap::new(),
            overflow: None,
            writer_memory: IN_FLIGHT_INDEX_WRITER.clone(),
        })
    }

    fn create_split(&self, partition: u64) -> anyhow::Result<SplitWriter> {
        let builder = IndexedSplitBuilder::from_doc_mapper(
            self.pipeline_id.clone(),
            partition,
            self.delete_opstamp,
            &self.mapper,
            &self.settings,
            self.directory.clone(),
            self.io_controls.clone().set_component("indexer"),
            self.clustering_enabled,
        )?;
        Ok(SplitWriter::new(builder, &self.writer_memory))
    }

    pub fn add_documents(&mut self, documents: Vec<Bytes>) -> anyhow::Result<()> {
        let mut processed = Vec::new();
        for document in documents {
            let _guard = self.io_controls.check_if_alive()?;
            self.converter.process_raw_doc(document, &mut processed);
            for document in processed.drain(..) {
                let partition = document.partition;
                if let Some(split) = self.splits.get_mut(&partition) {
                    split.add_document(document)?;
                } else if self.splits.len() < self.mapper.max_num_partitions().get() as usize {
                    let mut split = self.create_split(partition)?;
                    split.add_document(document)?;
                    self.splits.insert(partition, split);
                } else {
                    if self.overflow.is_none() {
                        self.overflow = Some(self.create_split(OTHER_PARTITION_ID)?);
                    }
                    self.overflow
                        .as_mut()
                        .context("missing overflow split")?
                        .add_document(document)?;
                }
            }
            self.io_controls.progress().record_progress();
        }
        Ok(())
    }

    /// Processes a whole file synchronously, preserving partially filled builders across files.
    /// Flushes serialize to scratch here. Uploads happen after this file has been consumed.
    pub fn index_file(
        &mut self,
        path: &Path,
        file: &DocumentsStagedForUpload,
        merge_policy: &Arc<dyn MergePolicy>,
        retention: Option<&RetentionPolicy>,
        last_file: bool,
        num_uploaded_splits: usize,
    ) -> anyhow::Result<Vec<PreparedSplit>> {
        let _guard = self.io_controls.check_if_alive()?;
        let index_uid = file
            .index_uid
            .as_ref()
            .context("assigned file has no index UID")?;
        let mut reader = DocumentFileReader::open(path, index_uid, file.num_bytes)?;
        let mut prepared = Vec::new();
        loop {
            let _guard = self.io_controls.check_if_alive()?;
            let documents = read_batch(&mut reader)?;
            let end_of_file = documents.is_empty();
            self.add_documents(documents)?;
            if self.should_flush() || (end_of_file && last_file) {
                prepared.extend(self.prepare_splits(
                    merge_policy,
                    retention,
                    num_uploaded_splits + prepared.len(),
                )?);
            }
            if end_of_file {
                break;
            }
        }
        Ok(prepared)
    }

    fn prepare_splits(
        &mut self,
        merge_policy: &Arc<dyn MergePolicy>,
        retention: Option<&RetentionPolicy>,
        num_existing_splits: usize,
    ) -> anyhow::Result<Vec<PreparedSplit>> {
        let splits = self.take_splits();
        ensure!(
            num_existing_splits + splits.len() <= 1000,
            "assigned group exceeds the publication split limit"
        );
        let mut prepared = Vec::with_capacity(splits.len());
        for split in splits {
            let io_controls = self.io_controls.clone().set_component("index_serializer");
            let _guard = io_controls.check_if_alive()?;
            let tags = self.tag_fields()?;
            split
                .builder
                .controlled_directory
                .set_io_controls(io_controls.clone());
            let packaged = create_packaged_split(split.finalize()?, &tags, io_controls.progress())?;
            let _guard = io_controls.check_if_alive()?;
            let (metadata, payload) = prepare_split_for_upload(&packaged, merge_policy, retention)?;
            prepared.push(PreparedSplit {
                packaged,
                metadata,
                payload,
            });
        }
        Ok(prepared)
    }

    pub fn should_flush(&self) -> bool {
        let mut num_docs = 0;
        let mut num_bytes = 0;
        for split in self.splits.values().chain(self.overflow.iter()) {
            num_docs += split.builder.split_attrs.num_docs;
            num_bytes += split.builder.mem_usage() as u64;
        }
        num_docs >= self.settings.split_num_docs_target as u64
            || num_bytes >= self.settings.resources.heap_size.as_u64()
    }

    pub fn take_splits(&mut self) -> Vec<SplitWriter> {
        self.splits
            .drain()
            .map(|(_, split)| split)
            .chain(self.overflow.take())
            .collect()
    }

    pub fn tag_fields(&self) -> anyhow::Result<Vec<quickwit_doc_mapper::NamedField>> {
        self.mapper.tag_named_fields()
    }
}

/// Read incrementally inside the file task. The final document can exceed the byte target.
fn read_batch(reader: &mut DocumentFileReader) -> anyhow::Result<Vec<Bytes>> {
    let mut documents = Vec::new();
    let mut num_bytes = 0;
    while documents.len() < 128 && num_bytes < 1024 * 1024 {
        let Some(document) = reader.read_document()? else {
            break;
        };
        num_bytes += document.len();
        documents.push(document);
    }
    Ok(documents)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use quickwit_config::DocsClusteringConfig;
    use quickwit_proto::types::{IndexUid, NodeId, PipelineUid};

    use super::*;
    use crate::ingesthree::run_indexing;

    #[test]
    fn test_partition_overflow_clustering_and_memory_flush() {
        let mut config = IndexConfig::for_test("partitions", "ram:///splits");
        config.doc_mapping.partition_key = Some("owner".into());
        config.doc_mapping.max_num_partitions = NonZeroU32::new(1).unwrap();
        config.indexing_settings.resources.heap_size = bytesize::ByteSize(1);
        let clustering: DocsClusteringConfig = serde_json::from_value(serde_json::json!([
            {"fingerprint": [{"kind": "raw", "path": "owner"}]}
        ]))
        .unwrap();
        let mut builders = SplitBuilders::new(
            config,
            IndexingPipelineId {
                node_id: NodeId::from_str("test-node"),
                index_uid: IndexUid::for_test("partitions", 1),
                source_id: "ingesthree".into(),
                pipeline_uid: PipelineUid::random(),
            },
            7,
            TempDirectory::for_test(),
            IoControls::default(),
            Some(Fingerprinter::new(&clustering)),
        )
        .unwrap();
        let writer_memory = Gauge::local();
        builders.writer_memory = writer_memory.clone();
        // Document IDs must continue across batches within each split.
        for owners in [["alice", "bob"], ["charlie", "alice"]] {
            let docs = owners
                .into_iter()
                .map(|owner| {
                    Bytes::from(
                        serde_json::json!({
                            "timestamp": "2024-01-01T00:00:00Z", "owner": owner,
                        })
                        .to_string(),
                    )
                })
                .collect();
            builders.add_documents(docs).unwrap();
        }
        assert!(builders.should_flush());
        assert_eq!(builders.splits.len(), 1);
        assert_eq!(
            builders
                .overflow
                .as_ref()
                .unwrap()
                .builder
                .split_attrs
                .partition_id,
            OTHER_PARTITION_ID
        );
        assert_eq!(
            builders
                .overflow
                .as_ref()
                .unwrap()
                .builder
                .split_attrs
                .num_docs,
            2
        );
        let expected_bytes: usize = builders
            .splits
            .values()
            .chain(builders.overflow.iter())
            .map(|split| split.builder.mem_usage())
            .sum();
        assert!(expected_bytes > 0);
        assert_eq!(writer_memory.get(), expected_bytes as f64);
        let splits = builders.take_splits();
        assert!(!builders.should_flush());
        assert_eq!(writer_memory.get(), expected_bytes as f64);
        for split in splits {
            assert_eq!(split.builder.split_attrs.num_docs, 2);
            assert!(split.builder.doc_id_clusterer_opt.is_some());
            let memory_before = writer_memory.get();
            let split_bytes = split.memory_usage.delta();
            let split = split.finalize().unwrap();
            assert_eq!(writer_memory.get(), memory_before - split_bytes);
            assert_eq!(split.index.reader().unwrap().searcher().num_docs(), 2);
        }
        assert_eq!(writer_memory.get(), 0.0);
    }

    fn builders_for_test(directory: TempDirectory, writer_memory: Gauge) -> SplitBuilders {
        let config = IndexConfig::for_test("writer-memory", "ram:///splits");
        let mut builders = SplitBuilders::new(
            config,
            IndexingPipelineId {
                node_id: NodeId::from_str("test-node"),
                index_uid: IndexUid::for_test("writer-memory", 1),
                source_id: "ingesthree".into(),
                pipeline_uid: PipelineUid::random(),
            },
            0,
            directory,
            IoControls::default(),
            None,
        )
        .unwrap();
        builders.writer_memory = writer_memory;
        builders
            .add_documents(vec![Bytes::from_static(
                br#"{"timestamp":"2024-01-01T00:00:00Z","body":"hello"}"#,
            )])
            .unwrap();
        builders
    }

    #[test]
    fn test_writer_memory_released_on_serialization_error() {
        let writer_memory = Gauge::local();
        let mut builders = builders_for_test(TempDirectory::for_test(), writer_memory.clone());
        let writer = builders.take_splits().pop().unwrap();
        assert_eq!(writer_memory.get(), writer.builder.mem_usage() as f64);
        assert!(writer_memory.get() > 0.0);
        builders.io_controls.kill();
        assert!(writer.finalize().is_err());
        assert_eq!(writer_memory.get(), 0.0);
    }

    #[tokio::test]
    async fn test_writer_memory_and_assignment_released_on_blocking_error_or_panic() {
        for panic in [false, true] {
            let writer_memory = Gauge::local();
            let active_assignments = Gauge::local();
            let directory = TempDirectory::for_test();
            let builders = builders_for_test(directory.clone(), writer_memory.clone());
            assert!(writer_memory.get() > 0.0);
            let assignment = Arc::new(GaugeGuard::new(&active_assignments, 1.0));
            let result: anyhow::Result<()> = run_indexing(directory, assignment, move || {
                let _builders = builders;
                if panic {
                    panic!("injected blocking panic");
                }
                anyhow::bail!("injected blocking error");
            })
            .await;
            assert!(result.is_err());
            assert_eq!(writer_memory.get(), 0.0);
            assert_eq!(active_assignments.get(), 0.0);
        }
    }

    #[tokio::test]
    async fn test_cancelled_indexing_keeps_scratch_memory_and_assignment_until_work_exits() {
        let directory = TempDirectory::for_test();
        let path = directory.path().to_path_buf();
        let (started, wait_started) = tokio::sync::oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let (finished, wait_finished) = tokio::sync::oneshot::channel();
        let work_path = path.clone();
        let writer_memory = Gauge::local();
        let mut builders = builders_for_test(directory.clone(), writer_memory.clone());
        let writer = builders.take_splits().pop().unwrap();
        drop(builders);
        let expected_bytes = writer.builder.mem_usage() as f64;
        assert!(expected_bytes > 0.0);
        let active_assignments = Gauge::local();
        let assignment = Arc::new(GaugeGuard::new(&active_assignments, 1.0));
        let task = tokio::spawn(run_indexing(directory, assignment, move || {
            started.send(()).unwrap();
            wait_release.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(work_path.try_exists().unwrap());
            drop(writer.finalize().unwrap());
            finished.send(()).unwrap();
            Ok(())
        }));
        wait_started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(active_assignments.get(), 1.0);
        assert_eq!(writer_memory.get(), expected_bytes);
        assert!(
            path.try_exists().unwrap(),
            "scratch deleted while synchronous work was running"
        );
        release.send(()).unwrap();
        wait_finished.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while path.try_exists().unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(active_assignments.get(), 0.0);
        assert_eq!(writer_memory.get(), 0.0);
    }
}
