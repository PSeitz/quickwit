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
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use quickwit_common::io::IoControls;
use quickwit_common::temp_dir::TempDirectory;
use quickwit_config::{IndexConfig, IndexingSettings, SourceInputFormat, build_doc_mapper};
use quickwit_doc_mapper::DocMapper;
use quickwit_proto::indexing::IndexingPipelineId;

use crate::actors::{DocumentConverter, OTHER_PARTITION_ID};
use crate::docs_clustering::Fingerprinter;
use crate::models::IndexedSplitBuilder;

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
    splits: HashMap<u64, IndexedSplitBuilder>,
    overflow: Option<IndexedSplitBuilder>,
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
        })
    }

    fn create_split(&self, partition: u64) -> anyhow::Result<IndexedSplitBuilder> {
        IndexedSplitBuilder::from_doc_mapper(
            self.pipeline_id.clone(),
            partition,
            self.delete_opstamp,
            &self.mapper,
            &self.settings,
            self.directory.clone(),
            self.io_controls.clone().set_component("indexer"),
            self.clustering_enabled,
        )
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

    pub fn should_flush(&self) -> bool {
        let mut num_docs = 0;
        let mut num_bytes = 0;
        for split in self.splits.values().chain(self.overflow.iter()) {
            num_docs += split.split_attrs.num_docs;
            num_bytes += split.mem_usage() as u64;
        }
        num_docs >= self.settings.split_num_docs_target as u64
            || num_bytes >= self.settings.resources.heap_size.as_u64()
    }

    pub fn take_splits(&mut self) -> Vec<IndexedSplitBuilder> {
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
            builders.overflow.as_ref().unwrap().split_attrs.partition_id,
            OTHER_PARTITION_ID
        );
        assert_eq!(builders.overflow.as_ref().unwrap().split_attrs.num_docs, 2);
        let splits = builders.take_splits();
        assert!(!builders.should_flush());
        for split in splits {
            assert_eq!(split.split_attrs.num_docs, 2);
            assert!(split.doc_id_clusterer_opt.is_some());
            let split = split.finalize().unwrap();
            assert_eq!(split.index.reader().unwrap().searcher().num_docs(), 2);
        }
    }

    #[tokio::test]
    async fn test_cancelled_indexing_keeps_scratch_until_work_exits() {
        let directory = TempDirectory::for_test();
        let path = directory.path().to_path_buf();
        let (started, wait_started) = tokio::sync::oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let (finished, wait_finished) = tokio::sync::oneshot::channel();
        let work_path = path.clone();
        let task = tokio::spawn(run_indexing(directory, move || {
            started.send(()).unwrap();
            wait_release.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(work_path.try_exists().unwrap());
            finished.send(()).unwrap();
            Ok(())
        }));
        wait_started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
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
    }
}
