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

use std::fs::{self, File};
use std::io::BufWriter;
use std::pin::pin;

use futures::poll;
use quickwit_config::{DocMapping, SearchSettings, build_doc_mapper};
use quickwit_proto::ingest::ParseFailureReason;

use super::*;

#[test]
fn test_capacity_accounts_for_queue_disk_reservations_and_closed_intake() {
    let mut admission = Admission {
        max_queue_bytes: 100,
        queue_bytes: 0,
        max_disk_bytes: 100,
        disk_bytes: 0,
        reserved_disk_bytes: 0,
        closed: false,
    };
    let mut tracker = CapacityTracker::new(100, 100);
    assert_eq!(admission.capacity_score(&mut tracker), 10);
    admission.queue_bytes = 95;
    assert_eq!(admission.capacity_score(&mut tracker), 0);
    admission.queue_bytes = 0;
    assert!(admission.capacity_score(&mut tracker) > 0);
    admission.disk_bytes = 90;
    admission.reserved_disk_bytes = 5;
    assert_eq!(admission.capacity_score(&mut tracker), 0);
    admission.reserved_disk_bytes = 0;
    // The inherited trend penalty can keep the score at zero above 5% headroom.
    assert_eq!(admission.capacity_score(&mut tracker), 0);
    admission.disk_bytes = 0;
    assert!(admission.capacity_score(&mut tracker) > 0);
    admission.closed = true;
    assert_eq!(admission.capacity_score(&mut tracker), 0);
    admission.closed = false;
    admission.max_disk_bytes = 0;
    assert_eq!(admission.capacity_score(&mut tracker), 0);
}

#[tokio::test(start_paused = true)]
async fn test_capacity_broadcast_updates_and_stops_with_writer() {
    let cluster = quickwit_cluster::create_cluster_for_test(
        Vec::new(),
        &["indexer"],
        &quickwit_cluster::ChitchatTransport::default(),
        true,
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ingester = Ingester::open(dir.path(), 100).unwrap();
    let (queue, receiver) = WriteQueue::channel(&ingester, 100);
    let mut broadcast = pin!(queue.broadcast_capacity(&cluster));
    // The first sample is immediate. Later samples cover saturation and recovery.
    for (elapsed_secs, queue_bytes, score) in [(0, 0, "10"), (5, 95, "0"), (5, 0, "10")] {
        queue.admission.lock().unwrap().queue_bytes = queue_bytes;
        tokio::time::advance(Duration::from_secs(elapsed_secs)).await;
        assert!(poll!(&mut broadcast).is_pending());
        assert_eq!(
            cluster
                .get_self_key_value("ingesthree_capacity")
                .await
                .as_deref(),
            Some(score)
        );
    }
    drop(receiver);
    broadcast.await;
    assert_eq!(
        cluster
            .get_self_key_value("ingesthree_capacity")
            .await
            .as_deref(),
        Some("0")
    );
}

const BUDGET: u64 = 1024 * 1024;

fn index() -> IndexUid {
    IndexUid::for_test("logs", 1)
}

fn batch() -> DocBatchV2 {
    DocBatchV2::for_test([r#"{"message":"hello"}"#])
}

fn mapper() -> Arc<DocMapper> {
    let mapping: DocMapping = serde_json::from_str(
        r#"{
        "mode": "strict",
        "field_mappings": [{"name": "message", "type": "text"}]
    }"#,
    )
    .unwrap();
    build_doc_mapper(&mapping, &SearchSettings::default()).unwrap()
}

// Byte usage: queue reservations, disk reservations, and retained files.
fn byte_usage(queue: &WriteQueue) -> (u64, u64, u64) {
    let admission = queue.admission.lock().unwrap();
    (
        admission.queue_bytes,
        admission.reserved_disk_bytes,
        admission.disk_bytes,
    )
}

#[tokio::test]
async fn test_validate_and_append_only_valid_documents() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let docs = DocBatchV2::for_test(["", "[]", r#"{"unknown":"value"}"#, r#"{"message":"hello"}"#]);
    let response = queue.append(index(), docs, Some(mapper())).await.unwrap();
    assert_eq!(response.num_docs, 1);
    assert_eq!(response.parse_failures.len(), 3);
    for (position, failure) in response.parse_failures.iter().enumerate() {
        assert_eq!(failure.doc_uid(), DocUid::for_test(position as u128));
        assert!(!failure.message.is_empty());
    }
    assert_eq!(
        response.parse_failures[0].reason(),
        ParseFailureReason::InvalidJson
    );
    assert_eq!(
        response.parse_failures[1].reason(),
        ParseFailureReason::InvalidJson
    );
    assert_eq!(
        response.parse_failures[2].reason(),
        ParseFailureReason::InvalidSchema
    );
    let files = queue.finish().await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(Some(files[0].file_id), response.file_id);
    assert_eq!(files[0].num_docs, 1);
    let (_, documents) = crate::ingesthree::tests::read_input(&files[0].path);
    assert_eq!(documents, vec![br#"{"message":"hello"}"#.to_vec()]);
    assert_eq!(byte_usage(&queue), (0, 0, files[0].num_bytes));
}

#[tokio::test]
async fn test_all_invalid_releases_admission_without_creating_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let response = queue
        .append(index(), DocBatchV2::for_test(["", "[]"]), Some(mapper()))
        .await
        .unwrap();
    assert_eq!(response.file_id, None);
    assert_eq!(response.num_docs, 0);
    assert_eq!(response.parse_failures.len(), 2);
    assert_eq!(byte_usage(&queue), (0, 0, 0));
    assert_eq!(
        fs::read_dir(dir.path().join("ingesthree")).unwrap().count(),
        1
    );
    queue
        .append(index(), batch(), Some(mapper()))
        .await
        .unwrap();
    assert_eq!(queue.finish().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_validation_can_be_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let response = queue
        .append(index(), DocBatchV2::for_test(["invalid JSON", ""]), None)
        .await
        .unwrap();
    assert_eq!(response.num_docs, 2);
    assert!(response.parse_failures.is_empty());
    let files = queue.finish().await.unwrap();
    assert_eq!(
        crate::ingesthree::tests::read_input(&files[0].path).1,
        vec![b"invalid JSON".to_vec(), Vec::new()]
    );
}

#[tokio::test]
async fn test_malformed_batches_do_not_stop_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let mut mismatched_ids = batch();
    mismatched_ids.doc_uids.clear();
    let mut truncated = batch();
    truncated.doc_lengths[0] += 1;
    let mut trailing_bytes = batch();
    trailing_bytes.doc_lengths[0] -= 1;
    let mut duplicate_ids = DocBatchV2::for_test(["{}", "{}"]);
    duplicate_ids.doc_uids[1] = duplicate_ids.doc_uids[0];
    for malformed in [
        DocBatchV2::default(),
        mismatched_ids,
        truncated,
        trailing_bytes,
        duplicate_ids,
    ] {
        assert!(matches!(
            queue.append(index(), malformed, None).await,
            Err(IngestError::InvalidRequest(_))
        ));
        assert_eq!(byte_usage(&queue), (0, 0, 0));
    }
    assert_eq!(
        queue.append(index(), batch(), None).await.unwrap().num_docs,
        1
    );
    queue.finish().await.unwrap();
}

#[tokio::test]
async fn test_queue_budget_and_drain_are_shared_by_clones() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue_bytes, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let (queue, receiver) = WriteQueue::channel(&core, queue_bytes);
    let clone = queue.clone();
    let mut first = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert_eq!(byte_usage(&queue), (queue_bytes, disk_bytes, 0));
    assert!(matches!(
        clone.append(index(), batch(), None).await,
        Err(IngestError::QueueFull)
    ));
    assert_eq!(byte_usage(&queue), (queue_bytes, disk_bytes, 0));
    let mut finish = pin!(clone.finish());
    assert!(poll!(&mut finish).is_pending());
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::Unavailable)
    ));
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    let response = first.await.unwrap();
    let files = finish.await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(Some(files[0].file_id), response.file_id);
    assert_eq!(byte_usage(&queue), (0, 0, files[0].num_bytes));
    assert!(Ingester::open(dir.path(), BUDGET).is_ok());
}

#[tokio::test]
async fn test_dequeue_keeps_byte_reservation_until_request_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue_bytes, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let (queue, mut receiver) = WriteQueue::channel(&core, queue_bytes);
    let mut request = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut request).is_pending());
    let processing = receiver.try_recv().unwrap();
    assert_eq!(byte_usage(&queue), (queue_bytes, disk_bytes, 0));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::QueueFull)
    ));
    drop(processing);
    assert!(matches!(request.await, Err(IngestError::Unavailable)));
    assert_eq!(byte_usage(&queue), (0, 0, 0));
    drop(receiver);
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::Unavailable)
    ));
    assert_eq!(byte_usage(&queue), (0, 0, 0));
}

#[tokio::test]
async fn test_disk_budget_includes_queued_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (_, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let core = Ingester::open(dir.path(), disk_bytes).unwrap();
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let mut first = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::StorageFull)
    ));
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    first.await.unwrap();
    assert_eq!(byte_usage(&queue), (0, 0, disk_bytes));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::StorageFull)
    ));
    queue.finish().await.unwrap();
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_reservations_settle_to_file_growth() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let (_, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let first_id = queue.append(index(), batch(), None).await.unwrap().file_id;
    assert_eq!(byte_usage(&queue), (0, 0, disk_bytes));
    assert_eq!(
        queue.append(index(), batch(), None).await.unwrap().file_id,
        first_id
    );
    let doc_size = batch().doc_buffer.len() as u64 + FRAME_OVERHEAD;
    assert_eq!(byte_usage(&queue), (0, 0, disk_bytes + doc_size));
    let files = queue.finish().await.unwrap();
    assert_eq!(files[0].num_bytes, disk_bytes + doc_size);
}

#[tokio::test]
async fn test_recovered_files_consume_disk_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    let id = core
        .append(&index(), &[Bytes::from_static(b"existing")])
        .unwrap();
    let files = core.finish().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, 0, None)
        .await
        .unwrap();
    assert_eq!(byte_usage(&queue), (0, 0, files[0].num_bytes));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::StorageFull)
    ));
    assert_eq!(queue.finish().await.unwrap()[0].file_id, id);
}

#[tokio::test]
async fn test_dropped_callers_and_handles_still_drain() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    {
        let mut request = pin!(queue.append(index(), batch(), None));
        assert!(poll!(&mut request).is_pending());
    }
    assert!(byte_usage(&queue).0 > 0);
    let admission = queue.admission.clone();
    drop(queue);
    write_batches(core, receiver, admission, None)
        .await
        .unwrap();
    let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(recovered.finished_files().len(), 1);
    assert_eq!(recovered.finished_files()[0].num_docs, 1);
}

#[tokio::test]
async fn test_io_failure_stops_writer_and_releases_pending_requests() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.append(&index(), &[Bytes::from_static(b"existing")])
        .unwrap();
    let active = core.active.get_mut(&index()).unwrap();
    active.writer = BufWriter::new(File::open(&active.input.path).unwrap());
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let mut failing = pin!(queue.append(index(), batch(), None));
    let mut queued = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut failing).is_pending());
    assert!(poll!(&mut queued).is_pending());
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    assert!(matches!(failing.await, Err(IngestError::Internal(_))));
    assert!(matches!(queued.await, Err(IngestError::Unavailable)));
    assert!(worker.await.unwrap().is_err());
    assert_eq!((byte_usage(&queue).0, byte_usage(&queue).1), (0, 0));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::Unavailable)
    ));
    let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(recovered.finished_files()[0].num_docs, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_admission_stays_within_both_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let (queue_bytes, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let core = Ingester::open(dir.path(), 3 * disk_bytes).unwrap();
    let (queue, receiver) = WriteQueue::channel(&core, 3 * queue_bytes);
    let (polled, mut observations) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let clone = queue.clone();
        let polled = polled.clone();
        tasks.push(tokio::spawn(async move {
            let mut request = pin!(clone.append(index(), batch(), None));
            match poll!(&mut request) {
                std::task::Poll::Ready(result) => {
                    polled.send(false).unwrap();
                    result
                }
                std::task::Poll::Pending => {
                    polled.send(true).unwrap();
                    request.await
                }
            }
        }));
    }
    let mut admitted = 0;
    for _ in 0..16 {
        admitted += usize::from(observations.recv().await.unwrap());
    }
    assert_eq!(admitted, 3);
    assert_eq!(byte_usage(&queue), (3 * queue_bytes, 3 * disk_bytes, 0));
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    let mut successful = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => successful += 1,
            Err(IngestError::QueueFull) => (),
            other => panic!("unexpected result: {other:?}"),
        }
    }
    assert_eq!(successful, 3);
    assert_eq!(queue.finish().await.unwrap()[0].num_docs, 3);
    worker.await.unwrap().unwrap();
}
