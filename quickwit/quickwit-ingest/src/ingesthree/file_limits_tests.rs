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
use std::io::{BufWriter, Write};
use std::path::Path;
use std::pin::pin;
use std::time::Duration;

use futures::poll;
use tokio::time::advance;

use super::*;
use crate::ingesthree::tests::read_input;

const BUDGET: u64 = 1024 * 1024;
const DOCUMENT: &str = "document";

fn index(incarnation: u128) -> IndexUid {
    IndexUid::for_test("logs", incarnation)
}

fn documents(count: usize) -> Vec<Bytes> {
    vec![Bytes::from_static(DOCUMENT.as_bytes()); count]
}

fn batch() -> DocBatchV2 {
    DocBatchV2::for_test([DOCUMENT])
}

fn file_size(index_uid: &IndexUid, num_docs: u64) -> u64 {
    MAGIC.len() as u64
        + FRAME_OVERHEAD
        + index_uid.to_string().len() as u64
        + num_docs * (FRAME_OVERHEAD + DOCUMENT.len() as u64)
}

fn input_path(data_dir: &Path, file_id: Ulid, extension: &str) -> PathBuf {
    data_dir
        .join("ingesthree")
        .join(format!("{file_id}.{extension}"))
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

async fn wait_for_finished_file(path: &Path) {
    // Observe blocking IO without advancing the paused Tokio clock. A wall-clock
    // timeout also catches a worker that never handles the expired deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !path.try_exists().unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} was not finished",
            path.display()
        );
        tokio::task::yield_now().await;
    }
}

#[test]
fn test_size_limit_keeps_whole_batches_and_counts_framing() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(core.file_limits.target_file_bytes, 100_000_000);
    assert_eq!(core.file_limits.max_file_age, Duration::from_secs(10));
    assert_eq!(core.file_limits.max_open_files, 1_024);
    let index_uid = index(1);
    core.file_limits.target_file_bytes = file_size(&index_uid, 2);

    let first_id = core.append(&index_uid, &documents(1)).unwrap();
    assert!(core.finished_files().is_empty());
    assert_eq!(core.append(&index_uid, &documents(1)).unwrap(), first_id);
    assert!(core.active.is_empty());
    assert_eq!(
        core.finished_files()[0].num_bytes,
        core.file_limits.target_file_bytes
    );
    assert_eq!(read_input(&core.finished_files()[0].path).1, documents(2));

    let large_id = core.append(&index_uid, &documents(3)).unwrap();
    assert_ne!(large_id, first_id);
    assert!(core.active.is_empty());
    assert_eq!(core.finished_files()[1].num_bytes, file_size(&index_uid, 3));
    assert_eq!(read_input(&core.finished_files()[1].path).1, documents(3));
    let next_id = core.append(&index_uid, &documents(1)).unwrap();
    assert_ne!(next_id, first_id);
    assert_ne!(next_id, large_id);
    assert_eq!(core.active.len(), 1);
    let retained = core.disk_bytes;
    let files = core.finish().unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(
        files.iter().map(|input| input.num_bytes).sum::<u64>(),
        retained
    );
}

#[test]
fn test_finished_file_requires_a_new_header_in_the_disk_budget() {
    let dir = tempfile::tempdir().unwrap();
    let index_uid = index(1);
    let mut core = Ingester::open(dir.path(), file_size(&index_uid, 2)).unwrap();
    let first_id = core.append(&index_uid, &documents(1)).unwrap();
    core.finish_file(&index_uid).unwrap();
    // Another document would fit in the old file, but its replacement needs a new header.
    assert!(core.append(&index_uid, &documents(1)).is_err());
    assert!(!core.requires_recovery);
    assert_eq!(core.disk_bytes, file_size(&index_uid, 1));
    assert!(core.active.is_empty());
    assert_eq!(core.finished_files()[0].file_id, first_id);

    core.max_disk_bytes = 2 * file_size(&index_uid, 1);
    let next_id = core.append(&index_uid, &documents(1)).unwrap();
    assert_ne!(first_id, next_id);
    assert_eq!(core.disk_bytes, core.max_disk_bytes);
    assert_eq!(core.finished_files()[0].file_id, first_id);
    assert_eq!(core.active[&index_uid].input.num_docs, 1);
}

#[tokio::test(start_paused = true)]
async fn test_open_file_limit_rejects_new_writers_without_finishing_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.file_limits.max_open_files = 2;
    let first_id = core.append(&index(1), &documents(1)).unwrap();
    advance(Duration::from_secs(1)).await;
    let second_id = core.append(&index(2), &documents(1)).unwrap();
    let retained = core.disk_bytes;
    let deadline = core.next_deadline();
    let originals: Vec<_> = core
        .active
        .values()
        .map(|active| {
            (
                active.input.path.clone(),
                fs::read(&active.input.path).unwrap(),
            )
        })
        .collect();

    let error = core.append(&index(3), &documents(1)).unwrap_err();
    assert!(error.is::<OpenFileLimit>());
    assert!(!core.requires_recovery);
    assert_eq!(core.disk_bytes, retained);
    assert_eq!(core.next_deadline(), deadline);
    assert_eq!(core.active.len(), 2);
    assert!(core.finished_files().is_empty());
    assert_eq!(
        fs::read_dir(dir.path().join("ingesthree")).unwrap().count(),
        3
    );
    for (path, original) in originals {
        assert_eq!(fs::read(path).unwrap(), original);
    }
    assert_eq!(core.append(&index(1), &documents(1)).unwrap(), first_id);
    assert_eq!(core.append(&index(2), &documents(1)).unwrap(), second_id);
    assert!(core.finished_files().is_empty());

    // Reaching the size target finishes a file, allowing another writer.
    core.file_limits.target_file_bytes = file_size(&index(1), 3);
    assert_eq!(core.append(&index(1), &documents(1)).unwrap(), first_id);
    assert_eq!(core.finished_files()[0].file_id, first_id);
    let third_id = core.append(&index(3), &documents(1)).unwrap();
    assert_eq!(core.active[&index(3)].input.file_id, third_id);
    assert_eq!(core.active.len(), 2);
    assert_eq!(core.finish().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn test_writer_loop_finishes_expired_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.file_limits.max_open_files = 1;
    let first_id = core.append(&index(1), &documents(1)).unwrap();
    advance(Duration::from_secs(10)).await;
    // A batch already being processed can finish in the current file after expiry.
    assert_eq!(core.append(&index(1), &documents(1)).unwrap(), first_id);
    assert_eq!(core.active.len(), 1);
    assert!(core.finished_files().is_empty());

    // The loop must finish the expired file before this queued request needs a writer.
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let core = FileWriter::new(core, queue.admission.clone());
    let mut next = pin!(queue.append(index(2), batch(), None));
    assert!(poll!(&mut next).is_pending());
    let mut finish = pin!(queue.finish());
    assert!(poll!(&mut finish).is_pending());
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    let next_id = next.await.unwrap().file_id;
    assert_ne!(next_id, Some(first_id));
    let files = finish.await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].file_id, first_id);
    assert_eq!(read_input(&files[0].path).1, documents(2));
    assert_eq!(Some(files[1].file_id), next_id);
    assert_eq!(read_input(&files[1].path).1, documents(1));
}

#[tokio::test(start_paused = true)]
async fn test_idle_file_is_finished_at_creation_deadline_without_releasing_disk_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let first_id = queue
        .append(index(1), batch(), None)
        .await
        .unwrap()
        .file_id
        .unwrap();
    let open_path = input_path(dir.path(), first_id, "open");
    let finished_path = open_path.with_extension("finished");
    let original = fs::read(&open_path).unwrap();
    let retained = byte_usage(&queue);
    advance(Duration::from_secs(9)).await;
    assert!(open_path.try_exists().unwrap());
    assert!(!finished_path.try_exists().unwrap());
    advance(Duration::from_secs(1)).await;
    wait_for_finished_file(&finished_path).await;
    assert!(!open_path.try_exists().unwrap());
    assert_eq!(fs::read(&finished_path).unwrap(), original);
    assert_eq!(byte_usage(&queue), retained);
    let files = queue.finish().await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_id, first_id);
}

#[tokio::test(start_paused = true)]
async fn test_continuous_appends_do_not_extend_the_creation_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let first_id = queue
        .append(index(1), batch(), None)
        .await
        .unwrap()
        .file_id
        .unwrap();
    for _ in 0..9 {
        advance(Duration::from_secs(1)).await;
        let response = queue.append(index(1), batch(), None).await.unwrap();
        assert_eq!(response.file_id, Some(first_id));
    }
    advance(Duration::from_secs(1)).await;
    let next_id = queue
        .append(index(1), batch(), None)
        .await
        .unwrap()
        .file_id
        .unwrap();
    assert_ne!(next_id, first_id);
    let files = queue.finish().await.unwrap();
    assert_eq!(files.len(), 2);
    let first = files
        .iter()
        .find(|input| input.file_id == first_id)
        .unwrap();
    assert_eq!(first.num_docs, 10);
    assert_eq!(read_input(&first.path).1, documents(10));
}

#[tokio::test(start_paused = true)]
async fn test_index_deadlines_are_independent_and_timer_restarts_after_idle() {
    let dir = tempfile::tempdir().unwrap();
    let queue = WriteQueue::open(dir.path().to_path_buf(), BUDGET, BUDGET, None)
        .await
        .unwrap();
    let first_id = queue
        .append(index(1), batch(), None)
        .await
        .unwrap()
        .file_id
        .unwrap();
    advance(Duration::from_secs(3)).await;
    let second_id = queue
        .append(index(2), batch(), None)
        .await
        .unwrap()
        .file_id
        .unwrap();
    advance(Duration::from_secs(7)).await;
    wait_for_finished_file(&input_path(dir.path(), first_id, "finished")).await;
    assert!(
        input_path(dir.path(), second_id, "open")
            .try_exists()
            .unwrap()
    );
    advance(Duration::from_secs(3)).await;
    wait_for_finished_file(&input_path(dir.path(), second_id, "finished")).await;

    let third_id = queue
        .append(index(1), batch(), None)
        .await
        .unwrap()
        .file_id
        .unwrap();
    assert_ne!(third_id, first_id);
    advance(Duration::from_secs(10)).await;
    wait_for_finished_file(&input_path(dir.path(), third_id, "finished")).await;
    assert_eq!(queue.finish().await.unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn test_open_file_limit_releases_reservations_and_keeps_processing() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.file_limits.max_open_files = 1;
    core.file_limits.target_file_bytes = file_size(&index(1), 2);
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let core = FileWriter::new(core, queue.admission.clone());
    let clone = queue.clone();
    let mut first = pin!(queue.append(index(1), batch(), None));
    let mut rejected = pin!(clone.append(index(2), batch(), None));
    let mut existing = pin!(queue.append(index(1), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert!(poll!(&mut rejected).is_pending());
    assert!(poll!(&mut existing).is_pending());
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    let first_id = first.await.unwrap().file_id;
    assert!(matches!(rejected.await, Err(IngestError::OpenFileLimit)));
    assert_eq!(existing.await.unwrap().file_id, first_id);
    assert_eq!(byte_usage(&queue), (0, 0, file_size(&index(1), 2)));
    assert_eq!(
        fs::read_dir(dir.path().join("ingesthree")).unwrap().count(),
        2
    );

    let next_id = clone.append(index(2), batch(), None).await.unwrap().file_id;
    assert_ne!(next_id, first_id);
    let files = queue.finish().await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(read_input(&files[0].path).1, documents(2));
    assert_eq!(read_input(&files[1].path).1, documents(1));
    assert_eq!(
        byte_usage(&queue),
        (0, 0, file_size(&index(1), 2) + file_size(&index(2), 1))
    );
}

#[tokio::test]
async fn test_finishing_files_at_size_limit_settles_reservations_and_drains_queued_batches() {
    let dir = tempfile::tempdir().unwrap();
    let index_uid = index(1);
    let (_, disk_bytes) = num_bytes(&index_uid, &batch()).unwrap();
    let mut core = Ingester::open(dir.path(), 2 * disk_bytes).unwrap();
    core.file_limits.target_file_bytes = disk_bytes;
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let core = FileWriter::new(core, queue.admission.clone());
    let mut first = pin!(queue.append(index_uid.clone(), batch(), None));
    let mut second = pin!(queue.append(index_uid.clone(), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert!(poll!(&mut second).is_pending());
    assert_eq!(byte_usage(&queue).1, 2 * disk_bytes);
    assert!(matches!(
        queue.append(index_uid, batch(), None).await,
        Err(IngestError::StorageFull)
    ));
    let mut finish = pin!(queue.finish());
    assert!(poll!(&mut finish).is_pending());
    let worker = tokio::spawn(write_batches(core, receiver, queue.admission.clone(), None));
    let first_id = first.await.unwrap().file_id;
    let second_id = second.await.unwrap().file_id;
    assert_ne!(first_id, second_id);
    let files = finish.await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|input| input.num_docs == 1));
    assert_eq!(byte_usage(&queue), (0, 0, 2 * disk_bytes));
    let recovered = Ingester::open(dir.path(), 0).unwrap();
    assert_eq!(recovered.finished_files().len(), 2);
    assert_eq!(recovered.disk_bytes, 2 * disk_bytes);
}

#[test]
fn test_finishing_file_at_size_limit_io_failure_requires_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.file_limits.target_file_bytes = file_size(&index(1), 2);
    let file_id = core.append(&index(1), &documents(1)).unwrap();
    let finished_path = input_path(dir.path(), file_id, "finished");
    // Make the rename fail after the batch has been flushed.
    fs::create_dir(&finished_path).unwrap();
    assert!(core.append(&index(1), &documents(1)).is_err());
    assert!(core.requires_recovery);
    assert!(core.append(&index(2), &documents(1)).is_err());
    fs::remove_dir(&finished_path).unwrap();
    drop(core);
    let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(recovered.finished_files()[0].file_id, file_id);
    // The complete unacknowledged batch is retained, as with append failures.
    assert_eq!(read_input(&finished_path).1, documents(2));
}

#[tokio::test(start_paused = true)]
async fn test_finishing_expired_files_precedes_queued_work_and_io_failure_stops_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    let file_id = core.append(&index(1), &documents(1)).unwrap();
    let active = core.active.get_mut(&index(1)).unwrap();
    active.writer = BufWriter::new(File::open(&active.input.path).unwrap());
    active.writer.write_all(b"unflushed bytes").unwrap();
    let retained = core.disk_bytes;
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let core = FileWriter::new(core, queue.admission.clone());
    let mut first = pin!(queue.append(index(2), batch(), None));
    let mut second = pin!(queue.append(index(2), batch(), None));
    assert!(poll!(&mut first).is_pending());
    assert!(poll!(&mut second).is_pending());
    advance(Duration::from_secs(10)).await;
    let error = write_batches(core, receiver, queue.admission.clone(), None)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("failed to finish local files"));
    assert!(matches!(first.await, Err(IngestError::Unavailable)));
    assert!(matches!(second.await, Err(IngestError::Unavailable)));
    assert!(matches!(
        queue.append(index(2), batch(), None).await,
        Err(IngestError::Unavailable)
    ));
    assert_eq!(byte_usage(&queue), (0, 0, retained));
    let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
    assert_eq!(recovered.finished_files().len(), 1);
    assert_eq!(recovered.finished_files()[0].file_id, file_id);
    assert_eq!(recovered.finished_files()[0].num_docs, 1);
}
