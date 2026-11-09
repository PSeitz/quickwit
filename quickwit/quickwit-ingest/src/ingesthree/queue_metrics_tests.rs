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
use quickwit_metrics::{Counter, Gauge, GaugeGuard};

use super::*;

const BUDGET: u64 = 1024 * 1024;

fn index() -> IndexUid {
    IndexUid::for_test("queue-metrics", 1)
}

fn batch() -> DocBatchV2 {
    DocBatchV2::for_test([r#"{"message":"hello"}"#])
}

fn channel(
    core: Ingester,
    max_queue_bytes: u64,
) -> (
    WriteQueue,
    mpsc::UnboundedReceiver<WriterRequest>,
    FileWriter,
) {
    let (queue, receiver) = WriteQueue::channel(&core, max_queue_bytes);
    {
        let mut admission = queue.admission.lock().unwrap();
        admission.metrics.disk_bytes = Gauge::local();
        admission.metrics.disk_bytes.set(core.disk_bytes as f64);
        admission.metrics.queue_limit_bytes = Gauge::local();
        admission
            .metrics
            .queue_limit_bytes
            .set(max_queue_bytes as f64);
        admission.metrics.disk_limit_bytes = Gauge::local();
        admission
            .metrics
            .disk_limit_bytes
            .set(core.max_disk_bytes as f64);
        admission.metrics.validation = ValidationMetrics::for_test();
        admission.metrics.queue_full = Counter::local();
        admission.metrics.storage_full = Counter::local();
        admission.metrics.open_file_limit = Counter::local();
        admission.metrics.unavailable = Counter::local();
    }
    let writer = FileWriter::new(core, queue.admission.clone());
    (queue, receiver, writer)
}

// Per-owner contributions and local snapshots isolate parallel tests.
fn usage(queue: &WriteQueue) -> (u64, u64, u64) {
    let admission = queue.admission.lock().unwrap();
    let metrics = &admission.metrics;
    assert_eq!(metrics.memory_bytes.delta(), metrics.queue_bytes.delta());
    (
        metrics.queue_bytes.delta() as u64,
        metrics.disk_reserved_bytes.delta() as u64,
        metrics.disk_bytes.get() as u64,
    )
}

fn validation_counts(metrics: &ValidationMetrics) -> (u64, u64, u64, u64) {
    (
        metrics.valid_docs.get(),
        metrics.invalid_docs.get(),
        metrics.valid_bytes.get(),
        metrics.invalid_bytes.get(),
    )
}

#[tokio::test]
async fn test_queue_metrics_follow_cancelled_callers_through_append_and_writer_release() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue_bytes, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let (queue, receiver, writer) = channel(core, BUDGET);
    {
        let mut request = pin!(queue.append(index(), batch(), None));
        assert!(poll!(&mut request).is_pending());
    }
    assert_eq!(usage(&queue), (queue_bytes, disk_bytes, 0));
    let worker = tokio::spawn(write_batches(
        writer,
        receiver,
        queue.admission.clone(),
        None,
    ));
    let files = queue.finish().await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(files[0].num_docs, 1);
    assert_eq!(usage(&queue), (0, 0, files[0].num_bytes));
    let admission = queue.admission.lock().unwrap();
    assert_eq!(admission.disk_bytes, files[0].num_bytes);
    assert_eq!(admission.metrics.validation.valid_docs.get(), 1);
    assert_eq!(admission.metrics.unavailable.get(), 0);
    assert_eq!(admission.metrics.queue_limit_bytes.get(), BUDGET as f64);
    assert_eq!(admission.metrics.disk_limit_bytes.get(), BUDGET as f64);
}

#[tokio::test]
async fn test_admission_metrics_count_queue_storage_and_unavailable_rejections_once() {
    let dir = tempfile::tempdir().unwrap();
    let (queue_bytes, disk_bytes) = num_bytes(&index(), &batch()).unwrap();
    let core = Ingester::open(dir.path(), disk_bytes).unwrap();
    let (queue, mut receiver, writer) = channel(core, queue_bytes);
    let mut first = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::QueueFull)
    ));
    let WriterRequest::Append(request) = receiver.try_recv().unwrap() else {
        panic!("expected append")
    };
    assert_eq!(usage(&queue), (queue_bytes, disk_bytes, 0));
    let writer = append_batch(writer, request).await.unwrap();
    first.await.unwrap();
    assert_eq!(usage(&queue), (0, 0, disk_bytes));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::StorageFull)
    ));
    drop(receiver);
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::Unavailable)
    ));
    let admission = queue.admission.lock().unwrap();
    assert_eq!(admission.metrics.queue_full.get(), 1);
    assert_eq!(admission.metrics.storage_full.get(), 1);
    assert_eq!(admission.metrics.unavailable.get(), 1);
    drop(admission);
    drop(writer);
}

#[tokio::test]
async fn test_open_file_rejection_counts_validation_before_append() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.file_limits.max_open_files = 0;
    let (queue, receiver, writer) = channel(core, BUDGET);
    let worker = tokio::spawn(write_batches(
        writer,
        receiver,
        queue.admission.clone(),
        None,
    ));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::OpenFileLimit)
    ));
    assert_eq!(usage(&queue), (0, 0, 0));
    {
        let admission = queue.admission.lock().unwrap();
        assert_eq!(admission.metrics.open_file_limit.get(), 1);
        assert_eq!(
            validation_counts(&admission.metrics.validation),
            (1, 0, batch().num_bytes() as u64, 0)
        );
    }
    queue.finish().await.unwrap();
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_dropped_receiver_releases_queued_metric_contributions() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, receiver, writer) = channel(core, BUDGET);
    let mut request = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut request).is_pending());
    assert!(usage(&queue).0 > 0);
    drop(receiver);
    assert!(matches!(request.await, Err(IngestError::Unavailable)));
    assert_eq!(usage(&queue), (0, 0, 0));
    // Accepted work losing its reply is a writer failure, not an admission rejection.
    assert_eq!(queue.admission.lock().unwrap().metrics.unavailable.get(), 0);
    drop(writer);
}

#[tokio::test]
async fn test_failed_enqueue_drops_reserved_metric_contributions() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, mut receiver, writer) = channel(core, BUDGET);
    let mut response = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut response).is_pending());
    let request = receiver.try_recv().unwrap();
    drop(receiver);
    // Exercise the send-error ownership path without a timing-dependent close race.
    let failed_send = queue.sender.send(request).unwrap_err();
    assert!(usage(&queue).0 > 0);
    drop(failed_send);
    assert!(matches!(response.await, Err(IngestError::Unavailable)));
    assert_eq!(usage(&queue), (0, 0, 0));
    drop(writer);
}

#[test]
fn test_recovered_disk_metrics_release_after_deletion_and_do_not_double_count_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.append(&index(), &[Bytes::from_static(b"retained")])
        .unwrap();
    let files = core.finish().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, _receiver, mut writer) = channel(core, BUDGET);
    assert_eq!(usage(&queue), (0, 0, files[0].num_bytes));
    {
        let admission = queue.admission.lock().unwrap();
        assert_eq!(admission.metrics.queue_limit_bytes.get(), BUDGET as f64);
        assert_eq!(admission.metrics.disk_limit_bytes.get(), BUDGET as f64);
    }
    // A failed local delete retains its charge until retry succeeds.
    let hidden = files[0].path.with_extension("hidden");
    fs::rename(&files[0].path, &hidden).unwrap();
    assert!(writer.delete_finished_file(files[0].file_id).is_err());
    assert_eq!(usage(&queue).2, files[0].num_bytes);
    fs::rename(hidden, &files[0].path).unwrap();
    drop(writer);
    assert_eq!(usage(&queue).2, files[0].num_bytes);
    // Reopening replaces the node-wide snapshot while old closed handles remain.
    let disk_bytes = queue.admission.lock().unwrap().metrics.disk_bytes.clone();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (reopened, _receiver, mut writer) = channel(core, BUDGET);
    reopened.admission.lock().unwrap().metrics.disk_bytes = disk_bytes;
    assert_eq!(usage(&reopened).2, files[0].num_bytes);
    writer.delete_finished_file(files[0].file_id).unwrap();
    assert_eq!(usage(&reopened).2, 0);
    // Cleanup by the old owner must preserve the new owner's disk snapshot.
    queue.admission.lock().unwrap().record_usage();
    assert_eq!(usage(&queue).2, 0);
    assert!(!files[0].path.try_exists().unwrap());
}

#[tokio::test]
async fn test_writer_failure_releases_reservations_and_preserves_disk_and_validation_counts() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.append(&index(), &[Bytes::from_static(b"retained")])
        .unwrap();
    let retained_bytes = core.disk_bytes;
    let active = core.active.get_mut(&index()).unwrap();
    active.writer = BufWriter::new(File::open(&active.input.path).unwrap());
    let (queue, receiver, writer) = channel(core, BUDGET);
    let mut first = pin!(queue.append(index(), batch(), None));
    let mut second = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert!(poll!(&mut second).is_pending());
    let worker = tokio::spawn(write_batches(
        writer,
        receiver,
        queue.admission.clone(),
        None,
    ));
    assert!(matches!(first.await, Err(IngestError::Internal(_))));
    assert!(matches!(second.await, Err(IngestError::Unavailable)));
    assert!(worker.await.unwrap().is_err());
    assert_eq!(usage(&queue), (0, 0, retained_bytes));
    let admission = queue.admission.lock().unwrap();
    assert_eq!(admission.metrics.validation.valid_docs.get(), 1);
    assert_eq!(admission.metrics.unavailable.get(), 0);
    assert_eq!(admission.metrics.queue_limit_bytes.get(), BUDGET as f64);
    assert_eq!(admission.metrics.disk_limit_bytes.get(), BUDGET as f64);
    assert_eq!(admission.metrics.capacity_score.delta(), 0.0);
}

#[tokio::test]
async fn test_disk_metric_retains_flushed_growth_when_finishing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.append(&index(), &[Bytes::from_static(b"retained")])
        .unwrap();
    core.file_limits.target_file_bytes = 1;
    let retained_bytes = core.disk_bytes + batch().doc_buffer.len() as u64 + FRAME_OVERHEAD;
    let finished_path = core.active[&index()].input.path.with_extension("finished");
    fs::create_dir(&finished_path).unwrap();
    let (queue, receiver, writer) = channel(core, BUDGET);
    let worker = tokio::spawn(write_batches(
        writer,
        receiver,
        queue.admission.clone(),
        None,
    ));
    assert!(matches!(
        queue.append(index(), batch(), None).await,
        Err(IngestError::Internal(_))
    ));
    assert!(worker.await.unwrap().is_err());
    assert_eq!(usage(&queue), (0, 0, retained_bytes));
    fs::remove_dir(finished_path).unwrap();
    let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(recovered.disk_bytes, retained_bytes);
}

#[tokio::test]
async fn test_blocking_append_keeps_reservations_after_awaiter_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, mut receiver, writer) = channel(core, BUDGET);
    let mut request = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut request).is_pending());
    let WriterRequest::Append(append) = receiver.try_recv().unwrap() else {
        panic!("expected append")
    };
    let AppendRequest {
        index_uid,
        batch,
        reply,
        reservation,
        ..
    } = append;
    let documents = ValidatedDocuments {
        documents: batch.docs().map(|(_, bytes)| bytes).collect(),
        reservation,
    };
    drop(batch);
    let (started, ready) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (finished, done) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv().unwrap();
            let writer = append_validated(writer, index_uid, documents, Vec::new(), reply).unwrap();
            drop(writer);
            finished.send(()).unwrap();
        })
        .await
        .unwrap();
    });
    ready.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(usage(&queue).0 > 0);
    assert!(usage(&queue).1 > 0);
    release.send(()).unwrap();
    done.await.unwrap();
    assert_eq!(request.await.unwrap().num_docs, 1);
    let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(recovered.finished_files()[0].num_docs, 1);
    assert_eq!(usage(&queue), (0, 0, recovered.disk_bytes));
}

#[tokio::test]
async fn test_panicking_work_releases_input_gauges_and_closes_admission() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), BUDGET).unwrap();
    let (queue, mut receiver, writer) = channel(core, BUDGET);
    let mut request = pin!(queue.append(index(), batch(), None));
    assert!(poll!(&mut request).is_pending());
    let append = receiver.try_recv().unwrap();
    let panicked = tokio::task::spawn_blocking(move || {
        let _writer = writer;
        let _append = append;
        panic!("injected writer panic");
    })
    .await
    .unwrap_err();
    assert!(panicked.is_panic());
    assert!(matches!(request.await, Err(IngestError::Unavailable)));
    assert_eq!(usage(&queue), (0, 0, 0));
    assert!(queue.admission.lock().unwrap().closed);
    assert_eq!(
        queue
            .admission
            .lock()
            .unwrap()
            .metrics
            .capacity_score
            .delta(),
        0.0
    );
}

#[test]
fn test_capacity_metric_tracks_samples_and_closed_intake() {
    let dir = tempfile::tempdir().unwrap();
    let core = Ingester::open(dir.path(), 100).unwrap();
    let (queue, _receiver, writer) = channel(core, 100);
    let mut tracker = CapacityTracker::new(100, 100);
    {
        let mut admission = queue.admission.lock().unwrap();
        assert_eq!(admission.capacity_score(&mut tracker), 10);
        assert_eq!(admission.metrics.capacity_score.delta(), 10.0);
        admission.reserved_disk_bytes = 95;
        assert_eq!(admission.capacity_score(&mut tracker), 0);
        assert_eq!(admission.metrics.capacity_score.delta(), 0.0);
        admission.reserved_disk_bytes = 0;
        admission.metrics.record_capacity(10);
        admission.close();
        assert_eq!(admission.metrics.capacity_score.delta(), 0.0);
    }
    drop(writer);
}

#[test]
fn test_queue_contributions_do_not_reset_other_owners() {
    let gauge = Gauge::local();
    let mut first = QueueMetrics::new(BUDGET, BUDGET, 0);
    let mut second = QueueMetrics::new(BUDGET, BUDGET, 0);
    first.queue_bytes = GaugeGuard::new(&gauge, 0.0);
    second.queue_bytes = GaugeGuard::new(&gauge, 0.0);
    first.record_usage(10, 0);
    second.record_usage(20, 0);
    assert_eq!(gauge.get(), 30.0);
    drop(first);
    assert_eq!(gauge.get(), 20.0);
    drop(second);
    assert_eq!(gauge.get(), 0.0);
}
