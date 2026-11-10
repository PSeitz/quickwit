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

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use quickwit_common::metrics::index_label;
use quickwit_common::split_file;
use quickwit_config::IndexConfig;
use quickwit_ingest::ingesthree::Ingester;
use quickwit_metastore::{
    CreateIndexRequestExt, FileBackedMetastore, ListSplitsRequestExt,
    MetastoreServiceStreamSplitsExt, Split, SplitState,
};
use quickwit_metrics::{Counter, counter, label_values};
use quickwit_proto::metastore::{
    CreateIndexRequest, DeleteQuery, DocumentsStagedForUpload, ListSplitsRequest,
    PublishFileSplitsResponse, RegisterFileForIndexing,
};
use quickwit_storage::RamStorage;
use tantivy::collector::Count;
use tantivy::query::AllQuery;
use tokio::sync::{Notify, Semaphore};
use tower::ServiceBuilder;
use tower::limit::GlobalConcurrencyLimitLayer;

use super::*;
use crate::get_tantivy_directory_from_split_bundle;
use crate::metrics::{
    PUBLISHED_SPLIT, PUBLISHED_SPLIT_BYTES_TOTAL, PUBLISHED_SPLIT_DOCS_TOTAL,
    PUBLISHED_SPLIT_UNCOMPRESSED_BYTES_TOTAL, PUBLISHED_SPLITS_TOTAL,
};

const DOC: &str = r#"{"timestamp":"2024-01-01T00:00:00Z","body":"hello world","owner":"alice"}"#;
const DOC_LATER: &str = r#"{"timestamp":"2024-01-01T00:00:02Z","body":"goodbye","owner":"bob"}"#;

fn assignment() -> AssignFilesForIndexing {
    AssignFilesForIndexing {
        max_num_files: 10,
        max_num_bytes: 10_000_000,
        retry_after_secs: 60,
    }
}

async fn setup(mut config: IndexConfig) -> (S3Indexer, IndexUid) {
    config.indexing_settings.resources.heap_size = bytesize::ByteSize::mb(100);
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
        // Keep metric assertions independent even when per-index labels are disabled.
        source_id: format!("ingesthree-{}", index_uid.index_id),
        metastore,
        storage_resolver: StorageResolver::for_test(),
        scratch_directory: TempDirectory::for_test(),
        split_cache: Arc::new(IndexingSplitCache::no_caching()),
        max_concurrent_split_uploads: 4,
        indexing_io_throughput_limiter: None,
        fingerprinter: None,
    };
    (worker, index_uid)
}

async fn register_file(
    worker: &mut S3Indexer,
    index_uid: &IndexUid,
    docs: &[&str],
) -> DocumentsStagedForUpload {
    let directory = TempDirectory::for_test();
    let mut ingester = Ingester::open(directory.path(), 10_000_000).unwrap();
    ingester
        .append(
            index_uid,
            &docs
                .iter()
                .map(|doc| Bytes::copy_from_slice(doc.as_bytes()))
                .collect::<Vec<_>>(),
        )
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
        .unwrap()
        .staged_file
        .unwrap()
}

async fn read_file(
    worker: &mut S3Indexer,
    file: &DocumentsStagedForUpload,
) -> DocumentsStagedForUpload {
    worker
        .metastore
        .register_file_for_indexing(RegisterFileForIndexing {
            index_uid: file.index_uid.clone(),
            file_id: file.file_id.clone(),
            file_uri: file.file_uri.clone(),
            num_bytes: file.num_bytes,
        })
        .await
        .unwrap()
        .staged_file
        .unwrap()
}

async fn list_splits(worker: &mut S3Indexer, index_uid: &IndexUid) -> Vec<Split> {
    worker
        .metastore
        .list_splits(ListSplitsRequest::try_from_index_uid(index_uid.clone()).unwrap())
        .await
        .unwrap()
        .collect_splits()
        .await
        .unwrap()
}

fn publication_counters(worker: &S3Indexer, index_uid: &IndexUid) -> [Counter; 4] {
    let labels = label_values!(
        PUBLISHED_SPLIT => index_label(&index_uid.index_id).to_string(),
        worker.source_id.clone(), "0"
    );
    [
        counter!(parent: PUBLISHED_SPLITS_TOTAL, labels: [labels.clone()]),
        counter!(parent: PUBLISHED_SPLIT_DOCS_TOTAL, labels: [labels.clone()]),
        counter!(parent: PUBLISHED_SPLIT_BYTES_TOTAL, labels: [labels.clone()]),
        counter!(parent: PUBLISHED_SPLIT_UNCOMPRESSED_BYTES_TOTAL, labels: [labels]),
    ]
}

fn assert_publication_counters(counters: &[Counter; 4], splits: &[SplitMetadata]) {
    assert_eq!(counters[0].get(), splits.len() as u64);
    assert_eq!(
        counters[1].get(),
        splits
            .iter()
            .map(|split| split.num_docs as u64)
            .sum::<u64>()
    );
    assert_eq!(
        counters[2].get(),
        splits
            .iter()
            .map(|split| split.footer_offsets.end)
            .sum::<u64>()
    );
    assert_eq!(
        counters[3].get(),
        splits
            .iter()
            .map(|split| split.uncompressed_docs_size_in_bytes)
            .sum::<u64>()
    );
}

#[tokio::test]
async fn test_files_publish_searchable_splits_and_completion_together() {
    let config = IndexConfig::for_test("files", "ram:///splits");
    let mapping_uid = config.doc_mapping.doc_mapping_uid;
    let (mut worker, index_uid) = setup(config).await;
    let counters = publication_counters(&worker, &index_uid);
    assert_publication_counters(&counters, &[]);
    let active_assignments = Gauge::local();
    let observed_activity = active_assignments.clone();
    worker.metastore = MetastoreServiceClient::tower()
        .stack_assign_files_for_indexing_layer(tower::util::MapRequestLayer::new(move |request| {
            assert_eq!(observed_activity.get(), 0.0, "polling is idle");
            request
        }))
        .build(worker.metastore);
    let first = register_file(&mut worker, &index_uid, &[DOC, "not JSON"]).await;
    let second = register_file(&mut worker, &index_uid, &[DOC_LATER]).await;
    let delete_opstamp = worker
        .metastore
        .create_delete_task(DeleteQuery {
            index_uid: Some(index_uid.clone()),
            query_ast: r#"{"match_all":{}}"#.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .opstamp;
    let result = worker
        .index_next_with_activity(assignment(), &active_assignments)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active_assignments.get(), 0.0);
    assert_publication_counters(&counters, &result.splits);
    assert!(result.published);
    assert_eq!(result.index_uid, index_uid);
    assert_eq!(result.splits.len(), 1);
    let split = &result.splits[0];
    assert_eq!(split.num_docs, 2);
    assert_eq!(split.doc_mapping_uid, mapping_uid);
    assert_eq!(split.delete_opstamp, delete_opstamp);
    assert_eq!(split.time_range, Some(1704067200..=1704067202));
    assert!(split.tags.contains("owner:alice"));
    assert!(split.tags.contains("owner:bob"));
    assert!(split.footer_offsets.start < split.footer_offsets.end);
    let first = read_file(&mut worker, &first).await;
    let second = read_file(&mut worker, &second).await;
    assert!(first.completed_timestamp.is_some());
    assert_eq!(first.publication_id, second.publication_id);
    assert!(first.publication_id.is_some());
    assert_eq!(
        list_splits(&mut worker, &index_uid).await[0].split_state,
        SplitState::Published
    );
    assert!(
        worker
            .index_next_with_activity(assignment(), &active_assignments)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(active_assignments.get(), 0.0);
    assert_publication_counters(&counters, &result.splits);

    let storage = worker
        .storage_resolver
        .resolve(&Uri::for_test("ram:///splits"))
        .await
        .unwrap();
    let filename = split_file(&split.split_id);
    let path = worker.scratch_directory.path().join(&filename);
    storage
        .copy_to_file(Path::new(&filename), &path)
        .await
        .unwrap();
    let directory = get_tantivy_directory_from_split_bundle(&path).unwrap();
    let index = tantivy::Index::open(directory).unwrap();
    let reader = index.reader().unwrap();
    assert_eq!(reader.searcher().search(&AllQuery, &Count).unwrap(), 2);
    let inputs = worker
        .storage_resolver
        .resolve(&Uri::for_test("ram:///inputs"))
        .await
        .unwrap();
    assert!(inputs.exists(Path::new(&first.file_id)).await.unwrap());
}

#[tokio::test]
async fn test_whole_file_task_flushes_and_indexes_all_documents() {
    let mut config = IndexConfig::for_test("whole-file", "ram:///splits");
    config.indexing_settings.split_num_docs_target = 100;
    let (mut worker, index_uid) = setup(config).await;
    let input = register_file(&mut worker, &index_uid, &vec![DOC; 300]).await;
    let result = worker.index_next(assignment()).await.unwrap().unwrap();
    assert!(result.published);
    assert_eq!(result.splits.len(), 3);
    assert_eq!(
        result
            .splits
            .iter()
            .map(|split| split.num_docs)
            .sum::<usize>(),
        300
    );
    assert!(
        read_file(&mut worker, &input)
            .await
            .completed_timestamp
            .is_some()
    );
    let storage = worker
        .storage_resolver
        .resolve(&Uri::for_test("ram:///splits"))
        .await
        .unwrap();
    let mut searchable_docs = 0;
    for split in &result.splits {
        let filename = split_file(&split.split_id);
        let path = worker.scratch_directory.path().join(&filename);
        storage
            .copy_to_file(Path::new(&filename), &path)
            .await
            .unwrap();
        let directory = get_tantivy_directory_from_split_bundle(&path).unwrap();
        let index = tantivy::Index::open(directory).unwrap();
        let reader = index.reader().unwrap();
        searchable_docs += reader.searcher().search(&AllQuery, &Count).unwrap();
    }
    assert_eq!(searchable_docs, 300);
    assert!(
        list_splits(&mut worker, &index_uid)
            .await
            .iter()
            .all(|split| split.split_state == SplitState::Published)
    );
}

#[tokio::test]
async fn test_document_target_flushes_without_completing_partial_groups() {
    let mut config = IndexConfig::for_test("flush", "ram:///splits");
    config.indexing_settings.split_num_docs_target = 1;
    let (mut worker, index_uid) = setup(config).await;
    let counters = publication_counters(&worker, &index_uid);
    let first = register_file(&mut worker, &index_uid, &[DOC]).await;
    let second = register_file(&mut worker, &index_uid, &[DOC_LATER]).await;
    let result = worker.index_next(assignment()).await.unwrap().unwrap();
    assert!(result.published);
    assert_eq!(result.splits.len(), 2);
    assert_publication_counters(&counters, &result.splits);
    assert!(result.splits.iter().all(|split| split.num_docs == 1));
    assert_eq!(
        read_file(&mut worker, &first).await.publication_id,
        read_file(&mut worker, &second).await.publication_id
    );
}

#[tokio::test]
async fn test_all_invalid_documents_complete_without_outputs() {
    let (mut worker, index_uid) = setup(IndexConfig::for_test("invalid", "ram:///splits")).await;
    let counters = publication_counters(&worker, &index_uid);
    let input = register_file(
        &mut worker,
        &index_uid,
        &["not JSON", r#"{"body":"missing timestamp"}"#],
    )
    .await;
    let result = worker.index_next(assignment()).await.unwrap().unwrap();
    assert!(result.published);
    assert!(result.splits.is_empty());
    assert_publication_counters(&counters, &[]);
    assert!(
        read_file(&mut worker, &input)
            .await
            .completed_timestamp
            .is_some()
    );
    assert!(list_splits(&mut worker, &index_uid).await.is_empty());
}

#[tokio::test]
async fn test_corrupt_input_preserves_group_and_uploaded_outputs() {
    let mut config = IndexConfig::for_test("corrupt", "ram:///splits");
    config.indexing_settings.split_num_docs_target = 1;
    let (mut worker, index_uid) = setup(config).await;
    let counters = publication_counters(&worker, &index_uid);
    let active_assignments = Gauge::local();
    let mut files = vec![
        register_file(&mut worker, &index_uid, &[DOC]).await,
        register_file(&mut worker, &index_uid, &[DOC_LATER]).await,
    ];
    files.sort_by(|left, right| {
        (left.create_timestamp, &left.file_id).cmp(&(right.create_timestamp, &right.file_id))
    });
    let storage = worker
        .storage_resolver
        .resolve(&Uri::for_test("ram:///inputs"))
        .await
        .unwrap();
    let path = Path::new(&files[1].file_id);
    let mut damaged = storage.get_all(path).await.unwrap().to_vec();
    *damaged.last_mut().unwrap() ^= 1;
    storage.put(path, Box::new(damaged.clone())).await.unwrap();
    assert!(
        worker
            .index_next_with_activity(assignment(), &active_assignments)
            .await
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    assert_eq!(active_assignments.get(), 0.0);
    assert_publication_counters(&counters, &[]);
    for file in &files {
        let stored = read_file(&mut worker, file).await;
        assert!(stored.processing_started_timestamp.is_some());
        assert!(stored.completed_timestamp.is_none());
    }
    let splits = list_splits(&mut worker, &index_uid).await;
    assert_eq!(splits.len(), 1);
    assert_eq!(splits[0].split_state, SplitState::Staged);
    assert_eq!(storage.get_all(path).await.unwrap().as_slice(), damaged);
}

#[tokio::test]
async fn test_committed_publication_retry_keeps_ids_and_outputs() {
    let (mut worker, index_uid) = setup(IndexConfig::for_test("retry", "ram:///splits")).await;
    let counters = publication_counters(&worker, &index_uid);
    let before_confirmation = counters.clone();
    let input = register_file(&mut worker, &index_uid, &[DOC]).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let first = Arc::new(AtomicBool::new(true));
    let layer = ServiceBuilder::new()
        .map_request(move |request: PublishFileSplits| {
            recorded.lock().unwrap().push(request.clone());
            request
        })
        .map_result(
            move |result: Result<PublishFileSplitsResponse, MetastoreError>| {
                let response = result?;
                assert_publication_counters(&before_confirmation, &[]);
                if first.swap(false, Ordering::SeqCst) {
                    return Err(MetastoreError::Unavailable("committed reply lost".into()));
                }
                Ok(response)
            },
        )
        .into_inner();
    worker.metastore = MetastoreServiceClient::tower()
        .stack_publish_file_splits_layer(layer)
        .build(worker.metastore);
    let result = worker.index_next(assignment()).await.unwrap().unwrap();
    assert!(result.published);
    assert_publication_counters(&counters, &result.splits);
    let publication_id = {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        requests[0].publication_id.clone()
    };
    assert_eq!(
        read_file(&mut worker, &input).await.publication_id,
        Some(publication_id)
    );
    assert_eq!(list_splits(&mut worker, &index_uid).await.len(), 1);
}

#[tokio::test]
async fn test_cancel_after_committed_publication_preserves_winner() {
    let (mut worker, index_uid) = setup(IndexConfig::for_test("cancel", "ram:///splits")).await;
    let counters = publication_counters(&worker, &index_uid);
    let active_assignments = Gauge::local();
    let work_activity = active_assignments.clone();
    let input = register_file(&mut worker, &index_uid, &[DOC]).await;
    let committed = Arc::new(Notify::new());
    let notify = committed.clone();
    let metastore = worker.metastore.clone();
    let layer = tower::util::MapResultLayer::new(
        move |result: Result<PublishFileSplitsResponse, MetastoreError>| {
            result?;
            notify.notify_one();
            Err::<PublishFileSplitsResponse, _>(MetastoreError::Unavailable(
                "committed reply lost".into(),
            ))
        },
    );
    worker.metastore = MetastoreServiceClient::tower()
        .stack_publish_file_splits_layer(layer)
        .build(metastore.clone());
    let mut observer = S3Indexer {
        metastore,
        ..worker_for_observation(&worker)
    };
    let handle = tokio::spawn(async move {
        worker
            .index_next_with_activity(assignment(), &work_activity)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), committed.notified())
        .await
        .unwrap();
    assert_eq!(active_assignments.get(), 1.0);
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    assert_eq!(active_assignments.get(), 0.0);
    // The lost reply prevents confirmation, even though the metastore committed the split.
    assert_publication_counters(&counters, &[]);
    assert!(
        read_file(&mut observer, &input)
            .await
            .completed_timestamp
            .is_some()
    );
    let splits = list_splits(&mut observer, &index_uid).await;
    assert_eq!(splits.len(), 1);
    assert_eq!(splits[0].split_state, SplitState::Published);
    let storage = observer
        .storage_resolver
        .resolve(&Uri::for_test("ram:///splits"))
        .await
        .unwrap();
    assert!(
        storage
            .exists(Path::new(&split_file(splits[0].split_id())))
            .await
            .unwrap()
    );
    assert!(observer.index_next(assignment()).await.unwrap().is_none());
}

fn worker_for_observation(worker: &S3Indexer) -> S3Indexer {
    S3Indexer {
        node_id: worker.node_id.clone(),
        source_id: worker.source_id.clone(),
        metastore: worker.metastore.clone(),
        storage_resolver: worker.storage_resolver.clone(),
        scratch_directory: worker.scratch_directory.clone(),
        split_cache: worker.split_cache.clone(),
        max_concurrent_split_uploads: worker.max_concurrent_split_uploads,
        indexing_io_throughput_limiter: None,
        fingerprinter: None,
    }
}

#[tokio::test]
async fn test_reassigned_worker_wins_and_loser_returns_no_published_outputs() {
    let (mut older, index_uid) = setup(IndexConfig::for_test("race", "ram:///splits")).await;
    let counters = publication_counters(&older, &index_uid);
    let active_assignments = Gauge::local();
    let older_activity = active_assignments.clone();
    let input = register_file(&mut older, &index_uid, &[DOC]).await;
    let mut newer = worker_for_observation(&older);
    let publication_gate = Arc::new(Semaphore::new(0));
    older.metastore = MetastoreServiceClient::tower()
        .stack_publish_file_splits_layer(GlobalConcurrencyLimitLayer::with_semaphore(
            publication_gate.clone(),
        ))
        .build(older.metastore);
    let handle = tokio::spawn(async move {
        older
            .index_next_with_activity(assignment(), &older_activity)
            .await
    });
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        // Wait for a staged split to establish that the older assignment has begun.
        while list_splits(&mut newer, &index_uid).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        loop {
            let request = AssignFilesForIndexing {
                retry_after_secs: 1,
                ..assignment()
            };
            if let Some(publication) = newer
                .index_next_with_activity(request, &active_assignments)
                .await
                .unwrap()
            {
                break publication;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(result.published);
    assert_eq!(active_assignments.get(), 1.0);
    assert_publication_counters(&counters, &result.splits);
    publication_gate.add_permits(1);
    let older = handle.await.unwrap().unwrap().unwrap();
    assert!(!older.published);
    assert!(older.splits.is_empty());
    assert_eq!(active_assignments.get(), 0.0);
    assert_publication_counters(&counters, &result.splits);
    let splits = list_splits(&mut newer, &index_uid).await;
    assert_eq!(
        splits
            .iter()
            .filter(|split| split.split_state == SplitState::Published)
            .count(),
        1
    );
    assert_eq!(
        splits
            .iter()
            .filter(|split| split.split_state == SplitState::Staged)
            .count(),
        1
    );
    assert!(
        read_file(&mut newer, &input)
            .await
            .completed_timestamp
            .is_some()
    );
}
