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

use std::time::Duration;

use anyhow::Context;
use quickwit_actors::Mailbox;
use quickwit_cluster::Cluster;
use quickwit_config::NodeConfig;
use quickwit_config::service::QuickwitService;
use quickwit_indexing::actors::IndexingService;
use quickwit_indexing::ingesthree::StopS3Indexers;
use quickwit_ingest::ingesthree::{Uploader, WriteQueue};
use quickwit_proto::metastore::MetastoreServiceClient;
use quickwit_storage::StorageResolver;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// File indexing publishes durable splits for the standalone planner to discover.
/// Router-only nodes do not produce splits and can use either compaction setting.
pub(crate) fn validate_config(config: &NodeConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        !config.ingest_api_config.enable_ingesthree
            || !config.is_service_enabled(QuickwitService::Indexer)
            || config.enable_standalone_compactors,
        "ingesthree-enabled indexers require enable_standalone_compactors=true"
    );
    Ok(())
}

/// Completes recovered handoffs before exposing intake. The upload prefix must remain
/// unchanged while this node retains local inputs, including across restarts.
pub(crate) async fn open(
    config: &NodeConfig,
    metastore: MetastoreServiceClient,
    storage_resolver: &StorageResolver,
) -> anyhow::Result<WriteQueue> {
    // A leading dot keeps the shared input prefix outside the index-name namespace.
    let input_uri = config.default_index_root_uri.join(".ingesthree")?;
    let storage = storage_resolver.resolve(&input_uri).await?;
    info!(%input_uri, "recovering ingesthree input files before opening intake");
    WriteQueue::open(
        config.data_dir_path.clone(),
        config.ingest_api_config.max_queue_memory_usage.as_u64(),
        config.ingest_api_config.max_queue_disk_usage.as_u64(),
        Some(Uploader::new(storage, metastore)),
    )
    .await
    .context("failed to open ingesthree intake")
}

/// Broadcasts capacity while watching intake. Unexpected closure fails the server,
/// allowing process restart to recover retained files.
pub(crate) async fn watch_writer(
    cluster: &Cluster,
    queue: Option<WriteQueue>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Ok(()),
        _ = async {
            match queue {
                Some(queue) => queue.broadcast_capacity(cluster).await,
                None => std::future::pending().await,
            }
        } => anyhow::bail!("ingesthree writer closed intake unexpectedly"),
    }
}

/// Closes intake and stops worker restarts as shutdown begins. Uploaded inputs remain
/// eligible for another indexer. A drain timeout retains local inputs for recovery.
pub(crate) async fn shutdown(
    queue: &WriteQueue,
    indexing_service: &Mailbox<IndexingService>,
    timeout: Duration,
) {
    let (worker_result, drain_result) = tokio::join!(
        tokio::time::timeout(timeout, indexing_service.ask(StopS3Indexers)),
        tokio::time::timeout(timeout, queue.drain()),
    );
    match worker_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => error!(?error, "failed to stop ingesthree indexing worker"),
        Err(error) => error!(?error, "timed out stopping ingesthree indexing worker"),
    }
    match drain_result {
        Ok(Ok(())) => info!("ingesthree intake drained"),
        Ok(Err(error)) => error!(
            ?error,
            "ingesthree drain failed; retained inputs need recovery"
        ),
        Err(error) => error!(
            ?error,
            "ingesthree drain timed out; retained inputs need recovery"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use futures::poll;
    use quickwit_common::uri::Uri;
    use quickwit_config::service::QuickwitService;
    use quickwit_config::{IndexConfig, NodeConfig};
    use quickwit_ingest::ingesthree::{
        DocumentFileReader, IngestError, Ingester, Uploader, WriteQueue,
    };
    use quickwit_metastore::{CreateIndexRequestExt, FileBackedMetastore};
    use quickwit_proto::ingest::DocBatchV2;
    use quickwit_proto::metastore::{
        AssignFilesForIndexing, CreateIndexRequest, MetastoreService, MetastoreServiceClient,
    };
    use quickwit_proto::types::IndexUid;
    use quickwit_storage::{LocalFileStorage, MockStorage, Storage, StorageErrorKind};
    use tokio_util::sync::CancellationToken;

    async fn watch_writer(
        queue: Option<WriteQueue>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let cluster = quickwit_cluster::create_cluster_for_test(
            Vec::new(),
            &["indexer"],
            &quickwit_cluster::ChitchatTransport::default(),
            true,
        )
        .await?;
        super::watch_writer(&cluster, queue, shutdown).await
    }

    #[tokio::test]
    async fn test_ingesthree_rejects_local_merging_before_server_startup() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::for_test();
        config.enabled_services = [QuickwitService::Indexer].into_iter().collect();
        config.ingest_api_config.enable_ingesthree = true;
        config.enable_standalone_compactors = false;
        config.data_dir_path = directory.path().join("unused");
        let error = crate::serve_quickwit(
            config,
            quickwit_common::runtimes::RuntimesConfig::light_for_tests(),
            quickwit_metastore::MetastoreResolver::unconfigured(),
            quickwit_storage::StorageResolver::unconfigured(),
            crate::tcp_listener::for_tests::TestTcpListenerResolver::default(),
            Box::pin(std::future::pending()),
            crate::do_nothing_env_filter_reload_fn(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ingesthree-enabled indexers require enable_standalone_compactors=true"
        );
        assert!(!directory.path().join("unused").try_exists().unwrap());
    }

    const BUDGET: u64 = 1024 * 1024;
    const DOCUMENT: &str = r#"{"body":"accepted before shutdown"}"#;

    // Keeps Tokio's paused clock from advancing while filesystem threads run.
    struct ClockGuard(tokio::task::JoinHandle<()>);

    impl ClockGuard {
        fn new() -> Self {
            Self(tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            }))
        }
    }

    impl Drop for ClockGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    // A wall-clock deadline also catches stalled operations while Tokio time is paused.
    async fn complete<T>(future: impl std::future::Future<Output = T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                result = &mut future => return result,
                _ = tokio::task::yield_now() => assert!(std::time::Instant::now() < deadline, "operation stalled"),
            }
        }
    }

    fn local_storage(path: &Path) -> Arc<dyn Storage> {
        let uri: Uri = format!("file://{}", path.display()).parse().unwrap();
        Arc::new(LocalFileStorage::from_uri(&uri).unwrap())
    }

    async fn append_document(
        queue: &WriteQueue,
        directory: &Path,
        index_uid: &IndexUid,
    ) -> PathBuf {
        let response =
            complete(queue.append(index_uid.clone(), DocBatchV2::for_test([DOCUMENT]), None))
                .await
                .unwrap();
        assert_eq!(response.num_docs, 1);
        assert!(response.parse_failures.is_empty());
        directory
            .join("ingesthree")
            .join(format!("{}.open", response.file_id.unwrap()))
    }

    async fn assert_recoverable(
        directory: &Path,
        accepted_path: &Path,
        index_uid: &IndexUid,
        accepted_bytes: &[u8],
    ) {
        // Channel closure can precede the blocking task that releases directory ownership.
        let ingester = complete(async {
            loop {
                match Ingester::open(directory, BUDGET) {
                    Ok(ingester) => return ingester,
                    Err(error) => assert_eq!(
                        error.to_string(),
                        "failed to exclusively lock ingesthree directory"
                    ),
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let recovered = ingester.finish().unwrap();
        assert_eq!(recovered.len(), 1);
        let file = &recovered[0];
        assert_eq!(file.path, accepted_path.with_extension("finished"));
        assert_eq!(&file.index_uid, index_uid);
        assert_eq!(file.num_docs, 1);
        assert_eq!(fs::read(&file.path).unwrap(), accepted_bytes);
        let mut reader = DocumentFileReader::open(&file.path, index_uid, file.num_bytes).unwrap();
        assert_eq!(
            reader.read_document().unwrap().unwrap(),
            DOCUMENT.as_bytes()
        );
        assert!(reader.read_document().unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_writer_io_failure_fails_watch_and_preserves_accepted_file() {
        let _clock = ClockGuard::new();
        let directory = tempfile::tempdir().unwrap();
        let queue = complete(WriteQueue::open(
            directory.path().to_path_buf(),
            BUDGET,
            BUDGET,
            None,
        ))
        .await
        .unwrap();
        let index_uid = IndexUid::for_test("logs", 1);
        let accepted_path = append_document(&queue, directory.path(), &index_uid).await;
        let accepted_bytes = fs::read(&accepted_path).unwrap();
        let shutdown = CancellationToken::new();
        let mut watch = Box::pin(watch_writer(Some(queue.clone()), shutdown.clone()));
        assert!(poll!(&mut watch).is_pending());

        // A directory at the rename destination causes a real IO failure, even as root.
        let finish_path = accepted_path.with_extension("finished");
        fs::create_dir(&finish_path).unwrap();
        tokio::time::advance(Duration::from_secs(11)).await;
        let error = complete(watch).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "ingesthree writer closed intake unexpectedly"
        );
        assert!(!shutdown.is_cancelled());
        assert!(matches!(
            complete(queue.append(index_uid.clone(), DocBatchV2::for_test([DOCUMENT]), None)).await,
            Err(IngestError::Unavailable)
        ));
        assert_eq!(fs::read(&accepted_path).unwrap(), accepted_bytes);

        fs::remove_dir(&finish_path).unwrap();
        assert_recoverable(
            directory.path(),
            &accepted_path,
            &index_uid,
            &accepted_bytes,
        )
        .await;
        // A watcher installed after the failure must also observe the closed channel.
        assert!(complete(watch_writer(Some(queue), shutdown)).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_permanent_handoff_failure_fails_watch_and_preserves_accepted_file() {
        let _clock = ClockGuard::new();
        let directory = tempfile::tempdir().unwrap();
        let metastore = MetastoreServiceClient::new(FileBackedMetastore::for_test(local_storage(
            &directory.path().join("metastore"),
        )));
        let mut storage = MockStorage::new();
        storage
            .expect_uri()
            .return_const(Uri::for_test("file:///uploads"));
        let (attempted, mut upload_attempt) = tokio::sync::oneshot::channel();
        storage.expect_put().once().return_once(move |_, _| {
            attempted.send(()).unwrap();
            Err(StorageErrorKind::Unauthorized.with_error(anyhow::anyhow!("upload denied")))
        });
        let queue = complete(WriteQueue::open(
            directory.path().to_path_buf(),
            BUDGET,
            BUDGET,
            Some(Uploader::new(Arc::new(storage), metastore)),
        ))
        .await
        .unwrap();
        let index_uid = IndexUid::for_test("logs", 1);
        let accepted_path = append_document(&queue, directory.path(), &index_uid).await;
        let accepted_bytes = fs::read(&accepted_path).unwrap();
        let shutdown = CancellationToken::new();
        let mut watch = Box::pin(watch_writer(Some(queue.clone()), shutdown.clone()));
        assert!(poll!(&mut watch).is_pending());

        // Expiry starts handoff without an explicit finish/drain closing intake first.
        tokio::time::advance(Duration::from_secs(11)).await;
        let error = complete(watch).await.unwrap_err();
        upload_attempt.try_recv().unwrap();
        assert_eq!(
            error.to_string(),
            "ingesthree writer closed intake unexpectedly"
        );
        assert!(!shutdown.is_cancelled());
        assert!(matches!(
            complete(queue.append(index_uid.clone(), DocBatchV2::for_test([DOCUMENT]), None)).await,
            Err(IngestError::Unavailable)
        ));
        assert_recoverable(
            directory.path(),
            &accepted_path,
            &index_uid,
            &accepted_bytes,
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_cancellation_and_drain_are_expected_shutdown() {
        let _clock = ClockGuard::new();
        for observe_after_drain in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let metastore = MetastoreServiceClient::new(FileBackedMetastore::for_test(
                local_storage(&directory.path().join("metastore")),
            ));
            let config = IndexConfig::for_test("logs", "file:///splits");
            let index_uid = complete(
                metastore.create_index(CreateIndexRequest::try_from_index_config(&config).unwrap()),
            )
            .await
            .unwrap()
            .index_uid()
            .clone();
            let upload_directory = directory.path().join("uploads");
            let queue = complete(WriteQueue::open(
                directory.path().to_path_buf(),
                BUDGET,
                BUDGET,
                Some(Uploader::new(
                    local_storage(&upload_directory),
                    metastore.clone(),
                )),
            ))
            .await
            .unwrap();
            let accepted_path = append_document(&queue, directory.path(), &index_uid).await;
            let accepted_bytes = fs::read(&accepted_path).unwrap();
            let shutdown = CancellationToken::new();
            let mut watch = Box::pin(watch_writer(Some(queue.clone()), shutdown.clone()));
            assert!(poll!(&mut watch).is_pending());

            // The server cancels this token before starting the queue drain.
            shutdown.cancel();
            if observe_after_drain {
                complete(queue.drain()).await.unwrap();
                // Both select branches are ready. Cancellation must take priority.
                complete(watch).await.unwrap();
            } else {
                let (watch_result, drain_result) =
                    complete(async { tokio::join!(watch, queue.drain()) }).await;
                watch_result.unwrap();
                drain_result.unwrap();
            }
            complete(queue.wait_for_closed()).await;
            let file_id = accepted_path.file_stem().unwrap().to_str().unwrap();
            assert_eq!(
                fs::read(upload_directory.join(file_id)).unwrap(),
                accepted_bytes
            );
            assert!(!accepted_path.try_exists().unwrap());
            assert!(
                !accepted_path
                    .with_extension("finished")
                    .try_exists()
                    .unwrap()
            );
            let assigned = complete(metastore.assign_files_for_indexing(AssignFilesForIndexing {
                max_num_files: 1,
                max_num_bytes: BUDGET,
                retry_after_secs: 60,
            }))
            .await
            .unwrap();
            assert_eq!(assigned.staged_files.len(), 1);
            assert_eq!(assigned.staged_files[0].file_id, file_id);
            assert_eq!(
                assigned.staged_files[0].index_uid.as_ref(),
                Some(&index_uid)
            );
            // Successful drain also releases ownership while queue handles remain alive.
            assert!(
                Ingester::open(directory.path(), BUDGET)
                    .unwrap()
                    .finish()
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
