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

use anyhow::ensure;
use async_trait::async_trait;
use quickwit_actors::{ActorContext, ActorExitStatus, Handler};
use quickwit_common::temp_dir;
use quickwit_config::INGEST_V2_SOURCE_ID;

use super::IndexingService;
use crate::ingesthree::{S3Indexer, S3IndexerSupervisor, StopS3Indexers};

impl IndexingService {
    pub(crate) fn enable_ingesthree(&mut self) -> anyhow::Result<()> {
        ensure!(
            self.merge_scheduler_service_opt.is_none(),
            "ingesthree requires standalone compactors"
        );
        self.ingesthree_enabled = true;
        Ok(())
    }

    pub(super) fn start_s3_indexers(&mut self) -> anyhow::Result<()> {
        if !self.ingesthree_enabled {
            return Ok(());
        }
        let num_workers = quickwit_common::num_cpus();
        ensure!(num_workers > 0, "S3 indexing requires at least one CPU");
        let scratch_directory = temp_dir::Builder::default()
            .join("ingesthree")
            .tempdir_in(&self.indexing_root_directory)?;
        let worker = S3Indexer {
            node_id: self.node_id.clone(),
            source_id: INGEST_V2_SOURCE_ID.to_string(),
            metastore: self.metastore.clone(),
            storage_resolver: self.storage_resolver.clone(),
            scratch_directory,
            split_cache: self.split_cache.clone(),
            max_concurrent_split_uploads: self.max_concurrent_split_uploads,
            indexing_io_throughput_limiter: self.indexing_io_throughput_limiter_opt.clone(),
            fingerprinter: self.fingerprinter_opt.clone(),
        };
        self.s3_indexers = (0..num_workers)
            .map(|_| S3IndexerSupervisor::spawn(worker.clone()))
            .collect();
        Ok(())
    }

    pub(super) async fn stop_s3_indexers(&mut self) {
        self.ingesthree_enabled = false;
        futures::future::join_all(self.s3_indexers.drain(..).map(S3IndexerSupervisor::stop)).await;
    }
}

#[async_trait]
impl Handler<StopS3Indexers> for IndexingService {
    type Reply = ();

    async fn handle(
        &mut self,
        _: StopS3Indexers,
        ctx: &ActorContext<Self>,
    ) -> Result<(), ActorExitStatus> {
        ctx.protect_future(self.stop_s3_indexers()).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use bytes::Bytes;
    use quickwit_actors::Universe;
    use quickwit_cluster::{ChitchatTransport, create_cluster_for_test};
    use quickwit_common::temp_dir::TempDirectory;
    use quickwit_common::uri::Uri;
    use quickwit_config::INGEST_V2_SOURCE_ID;
    use quickwit_config::merge_policy_config::{MergePolicyConfig, StableLogMergePolicyConfig};
    use quickwit_ingest::ingesthree::Ingester;
    use quickwit_metastore::{
        CreateIndexRequestExt, FileBackedMetastore, MetastoreServiceStreamSplitsExt,
    };
    use quickwit_proto::metastore::{
        AssignFilesForIndexing, CreateIndexRequest, MockMetastoreService, RegisterFileForIndexing,
    };
    use quickwit_storage::RamStorage;

    use super::super::*;
    use crate::ingesthree::{S3Indexer, StopS3Indexers};

    const DOC: &[u8] = br#"{"timestamp":"2024-01-01T00:00:00Z","body":"hello","owner":"alice"}"#;

    async fn fixture() -> (S3Indexer, IndexUid) {
        let mut config = IndexConfig::for_test("file-service", "ram:///splits");
        config.indexing_settings.resources.heap_size = ByteSize::mb(100);
        config.indexing_settings.merge_policy =
            MergePolicyConfig::StableLog(StableLogMergePolicyConfig {
                merge_factor: 2,
                max_merge_factor: 2,
                ..Default::default()
            });
        let metastore = MetastoreServiceClient::new(FileBackedMetastore::for_test(Arc::new(
            RamStorage::default(),
        )));
        let index_uid = metastore
            .create_index(CreateIndexRequest::try_from_index_config(&config).unwrap())
            .await
            .unwrap()
            .index_uid()
            .clone();
        let worker = S3Indexer {
            node_id: NodeId::from_str("test-node"),
            source_id: INGEST_V2_SOURCE_ID.to_string(),
            metastore,
            storage_resolver: StorageResolver::for_test(),
            scratch_directory: TempDirectory::for_test(),
            split_cache: Arc::new(IndexingSplitCache::no_caching()),
            max_concurrent_split_uploads: 2,
            indexing_io_throughput_limiter: None,
            fingerprinter: None,
        };
        (worker, index_uid)
    }

    async fn register(worker: &mut S3Indexer, index_uid: &IndexUid) {
        let directory = TempDirectory::for_test();
        let mut ingester = Ingester::open(directory.path(), 1_000_000).unwrap();
        ingester
            .append(index_uid, &[Bytes::from_static(DOC)])
            .unwrap();
        let file = ingester.finish().unwrap().pop().unwrap();
        let storage = worker
            .storage_resolver
            .resolve(&Uri::for_test("ram:///inputs"))
            .await
            .unwrap();
        storage
            .put(
                Path::new(&file.file_id.to_string()),
                Box::new(std::fs::read(&file.path).unwrap()),
            )
            .await
            .unwrap();
        worker
            .metastore
            .register_file_for_indexing(RegisterFileForIndexing {
                index_uid: Some(index_uid.clone()),
                file_id: file.file_id.to_string(),
                file_uri: storage
                    .uri()
                    .join(file.file_id.to_string())
                    .unwrap()
                    .to_string(),
                num_bytes: file.num_bytes,
            })
            .await
            .unwrap();
    }

    async fn new_service(directory: &Path, worker: &S3Indexer) -> IndexingService {
        let transport = ChitchatTransport::default();
        let cluster = create_cluster_for_test(Vec::new(), &["indexer"], &transport, true)
            .await
            .unwrap();
        IndexingService::new(
            worker.node_id.clone(),
            directory.to_path_buf(),
            IndexerConfig::for_test().unwrap(),
            1,
            cluster,
            worker.metastore.clone(),
            None,
            None,
            IngesterPool::default(),
            worker.storage_resolver.clone(),
            EventBroker::default(),
            worker.split_cache.clone(),
            None,
        )
        .await
        .unwrap()
    }

    async fn spawn_service(
        directory: &Path,
        universe: &Universe,
        worker: &S3Indexer,
        enabled: bool,
    ) -> (Mailbox<IndexingService>, ActorHandle<IndexingService>) {
        let mut service = new_service(directory, worker).await;
        if enabled {
            service.enable_ingesthree().unwrap();
        }
        universe.spawn_builder().spawn(service)
    }

    #[derive(Debug)]
    struct InspectS3Indexers;

    #[async_trait]
    impl Handler<InspectS3Indexers> for IndexingService {
        type Reply = usize;

        async fn handle(
            &mut self,
            _: InspectS3Indexers,
            _: &ActorContext<Self>,
        ) -> Result<Self::Reply, ActorExitStatus> {
            Ok(self.s3_indexers.len())
        }
    }

    async fn published_splits(worker: &mut S3Indexer, index_uid: &IndexUid) -> Vec<SplitMetadata> {
        let query =
            ListSplitsQuery::for_index(index_uid.clone()).with_split_state(SplitState::Published);
        worker
            .metastore
            .list_splits(ListSplitsRequest::try_from_list_splits_query(&query).unwrap())
            .await
            .unwrap()
            .collect_splits_metadata()
            .await
            .unwrap()
    }

    async fn wait_for_publications(
        worker: &mut S3Indexer,
        index_uid: &IndexUid,
        num_splits: usize,
    ) -> Vec<SplitMetadata> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let splits = published_splits(worker, index_uid).await;
                if splits.len() == num_splits {
                    return splits;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn file_indexing_requires_standalone_compactors() {
        let (worker, _) = fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let universe = Universe::new();
        let mut service = new_service(directory.path(), &worker).await;
        service.merge_scheduler_service_opt = Some(universe.get_or_spawn_one());
        let error = service.enable_ingesthree().unwrap_err();
        assert!(error.to_string().contains("standalone compactors"));
        assert!(!service.ingesthree_enabled);
        assert!(service.s3_indexers.is_empty());
        universe.assert_quit().await;
    }

    #[tokio::test]
    async fn file_indexing_is_opt_in_and_stop_is_idempotent() {
        let (mut worker, index_uid) = fixture().await;
        register(&mut worker, &index_uid).await;
        let directory = tempfile::tempdir().unwrap();
        let universe = Universe::new();
        let (service, _) = spawn_service(directory.path(), &universe, &worker, false).await;
        assert_eq!(service.ask(InspectS3Indexers).await.unwrap(), 0);
        assert!(published_splits(&mut worker, &index_uid).await.is_empty());
        service.ask(StopS3Indexers).await.unwrap();
        service.ask(StopS3Indexers).await.unwrap();
        universe.assert_quit().await;
    }

    #[tokio::test]
    async fn one_polling_task_per_cpu() {
        let (worker, _) = fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let mut service = new_service(directory.path(), &worker).await;
        let (sender, mut polls) = tokio::sync::mpsc::unbounded_channel();
        let mut metastore = MockMetastoreService::new();
        metastore
            .expect_assign_files_for_indexing()
            .returning(move |request| {
                assert_eq!(request.max_num_files, 1);
                sender.send(tokio::task::id()).unwrap();
                Ok(Default::default())
            });
        service.metastore = MetastoreServiceClient::from_mock(metastore);
        service.enable_ingesthree().unwrap();
        service.start_s3_indexers().unwrap();
        let num_workers = quickwit_common::num_cpus();
        assert_eq!(service.s3_indexers.len(), num_workers);
        // Count distinct running tasks, rather than repeated polls from one worker.
        let mut task_ids = std::collections::HashSet::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while task_ids.len() < num_workers {
                task_ids.insert(polls.recv().await.unwrap());
            }
        })
        .await
        .unwrap();
        service.stop_s3_indexers().await;
        assert!(service.s3_indexers.is_empty());
    }

    #[tokio::test]
    async fn polling_publishes_without_local_merges_and_stops() {
        let (mut worker, index_uid) = fixture().await;
        register(&mut worker, &index_uid).await;
        register(&mut worker, &index_uid).await;
        let directory = tempfile::tempdir().unwrap();
        let universe = Universe::new();
        let (service, handle) = spawn_service(directory.path(), &universe, &worker, true).await;
        let splits = wait_for_publications(&mut worker, &index_uid, 2).await;
        for split in splits {
            assert_eq!(split.num_docs, 1);
            assert_eq!(split.num_merge_ops, 0);
            assert_eq!(split.source_id, INGEST_V2_SOURCE_ID);
            assert_eq!(split.node_id, worker.node_id.as_str());
        }
        assert_eq!(
            service.ask(InspectS3Indexers).await.unwrap(),
            quickwit_common::num_cpus()
        );
        service.ask(SuperviseLoop).await.unwrap();
        let counters = handle.observe().await;
        assert_eq!(counters.num_running_pipelines, 0);
        assert_eq!(counters.num_running_merge_pipelines, 0);
        service.ask(StopS3Indexers).await.unwrap();
        assert_eq!(service.ask(InspectS3Indexers).await.unwrap(), 0);
        register(&mut worker, &index_uid).await;
        // An assignment remains available after stop, even after supervision runs again.
        service.ask(SuperviseLoop).await.unwrap();
        let remaining = worker
            .index_next(AssignFilesForIndexing {
                max_num_files: 1,
                max_num_bytes: 1_000_000,
                retry_after_secs: 60,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(remaining.published);
        assert_eq!(remaining.splits.len(), 1);
        assert_eq!(remaining.splits[0].num_docs, 1);
        assert_eq!(published_splits(&mut worker, &index_uid).await.len(), 3);
        service.ask(StopS3Indexers).await.unwrap();
        universe.assert_quit().await;
    }
}
