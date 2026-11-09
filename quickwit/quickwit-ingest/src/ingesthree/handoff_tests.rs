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

use std::fs;
use std::path::Path;
use std::time::Duration;

use futures::executor::block_on;
use futures::poll;
use quickwit_common::uri::Uri;
use quickwit_proto::metastore::{
    DocumentsStagedForUpload as RegisteredFile, EntityKind, MetastoreError, MetastoreServiceClient,
    MockMetastoreService, RegisterFileForIndexing, RegisterFileForIndexingResponse,
};
use quickwit_storage::{MockStorage, Storage, StorageErrorKind};
use tokio::time::advance;
use tower::limit::GlobalConcurrencyLimitLayer;

use super::*;

const BUDGET: u64 = 1024 * 1024;
const DOCUMENT: &str = "opaque\0document\n";

// Prevent automatic clock advancement while filesystem/validation threads run.
// Each test explicitly advances retry time. Drop also cleans up after assertion failures.
struct ClockGuard(tokio::task::JoinHandle<()>);

impl Drop for ClockGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn keep_clock_fixed() -> ClockGuard {
    ClockGuard(tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    }))
}

type UploadAttempts = mpsc::UnboundedReceiver<(PathBuf, Vec<u8>)>;

fn capture_uploads(
    outcomes: Vec<Result<(), StorageErrorKind>>,
) -> (Arc<MockStorage>, UploadAttempts) {
    let (attempted, attempts) = mpsc::unbounded_channel();
    let mut outcomes = outcomes.into_iter();
    let mut storage = MockStorage::new();
    storage
        .expect_uri()
        .return_const(Uri::for_test("ram:///bucket/prefix"));
    storage
        .expect_put()
        .times(outcomes.len())
        .returning(move |key, payload| {
            // MockStorage callbacks are synchronous. The real payload reads through
            // blocking filesystem workers, so consuming it here exercises file streaming.
            let bytes = block_on(payload.read_all()).unwrap().to_vec();
            attempted.send((key.to_path_buf(), bytes)).unwrap();
            outcomes
                .next()
                .unwrap()
                .map_err(|kind| kind.with_error(anyhow::anyhow!("upload failed")))
        });
    (Arc::new(storage), attempts)
}

fn ingester_with_finished_file(dir: &Path) -> (Ingester, DocumentsStagedForUpload) {
    let mut core = Ingester::open(dir, BUDGET).unwrap();
    let index_uid = IndexUid::for_test("logs", 1);
    core.append(&index_uid, &[Bytes::from_static(DOCUMENT.as_bytes())])
        .unwrap();
    core.finish_file(&index_uid).unwrap();
    let file = core.finished_files()[0].clone();
    (core, file)
}

fn start_writer(
    core: Ingester,
    storage: Arc<dyn Storage>,
    mock: MockMetastoreService,
) -> (WriteQueue, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let worker = tokio::spawn(write_batches(
        core,
        receiver,
        queue.admission.clone(),
        Some(Uploader::new(
            storage,
            MetastoreServiceClient::from_mock(mock),
        )),
    ));
    (queue, worker)
}

fn registration_response(request: RegisterFileForIndexing) -> RegisterFileForIndexingResponse {
    RegisterFileForIndexingResponse {
        staged_file: Some(RegisteredFile {
            index_uid: request.index_uid,
            file_id: request.file_id,
            file_uri: request.file_uri,
            num_bytes: request.num_bytes,
            create_timestamp: 100,
            ..Default::default()
        }),
    }
}

fn retained_bytes(queue: &WriteQueue) -> u64 {
    queue.admission.lock().unwrap().disk_bytes
}

// Paused Tokio time must not turn an infinite retry into a hanging test.
async fn complete<T>(future: impl std::future::Future<Output = T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = tokio::task::yield_now() => assert!(std::time::Instant::now() < deadline, "operation stalled"),
        }
    }
}

async fn wait_for_release(queue: &WriteQueue) {
    complete(async {
        while retained_bytes(queue) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn reopen(dir: &Path) -> Ingester {
    complete(async {
        loop {
            if let Ok(core) = Ingester::open(dir, BUDGET) {
                return core;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

async fn open_queue(dir: &Path, uploader: Option<Uploader>) -> Result<WriteQueue, IngestError> {
    complete(WriteQueue::open(
        dir.to_path_buf(),
        BUDGET,
        BUDGET,
        uploader,
    ))
    .await
}

async fn append(
    queue: &WriteQueue,
    index_id: &str,
    document: &'static str,
) -> Result<AppendResponse, IngestError> {
    queue
        .append(
            IndexUid::for_test(index_id, 1),
            DocBatchV2::for_test([document]),
            None,
        )
        .await
}

async fn assert_closed(queue: &WriteQueue) {
    assert!(matches!(
        append(queue, "logs", DOCUMENT).await,
        Err(IngestError::Unavailable)
    ));
}

fn stalled_metastore(
    mock: MockMetastoreService,
) -> (MetastoreServiceClient, Arc<tokio::sync::Semaphore>) {
    let permits = Arc::new(tokio::sync::Semaphore::new(0));
    let metastore = MetastoreServiceClient::tower()
        .stack_register_file_for_indexing_layer(GlobalConcurrencyLimitLayer::with_semaphore(
            permits.clone(),
        ))
        .build_from_mock(mock);
    (metastore, permits)
}

#[tokio::test(start_paused = true)]
async fn test_handoff_after_file_deadline_releases_capacity() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let mut core = Ingester::open(dir.path(), BUDGET).unwrap();
    core.file_limits.max_file_age = Duration::from_secs(1);
    let mut files = Vec::new();
    for index_id in ["logs", "traces"] {
        let index_uid = IndexUid::for_test(index_id, 1);
        core.append(&index_uid, &[Bytes::copy_from_slice(index_id.as_bytes())])
            .unwrap();
        let file = core.active[&index_uid].input.clone();
        let bytes = fs::read(&file.path).unwrap();
        files.push((file, bytes));
    }
    core.max_disk_bytes = core.disk_bytes;
    let total_bytes = core.disk_bytes;
    let (storage, mut uploads) = capture_uploads(vec![Ok(()), Ok(())]);
    let expected_files = files.clone();
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .times(2)
        .returning(move |request| {
            let (file, bytes) = expected_files
                .iter()
                .find(|(file, _)| file.file_id.to_string() == request.file_id)
                .unwrap();
            assert_eq!(request.index_uid, Some(file.index_uid.clone()));
            assert_eq!(
                request.file_uri,
                format!("ram:///bucket/prefix/{}", file.file_id)
            );
            assert_eq!(request.num_bytes, bytes.len() as u64);
            Ok(registration_response(request))
        });
    let (queue, worker) = start_writer(core, storage, mock);
    assert!(uploads.try_recv().is_err());
    advance(Duration::from_secs(2)).await;
    let mut expected_uploads: Vec<_> = files
        .iter()
        .map(|(file, bytes)| (PathBuf::from(file.file_id.to_string()), bytes.clone()))
        .collect();
    for _ in 0..files.len() {
        let upload = uploads.recv().await.unwrap();
        let position = expected_uploads
            .iter()
            .position(|expected| *expected == upload)
            .unwrap();
        expected_uploads.remove(position);
    }
    wait_for_release(&queue).await;
    for (file, _) in &files {
        assert!(!file.path.with_extension("finished").try_exists().unwrap());
    }
    // Refill the entire budget. Both the shared admission counter and the core's
    // counter must have released the old files for these writes to succeed.
    for index_id in ["logs", "traces"] {
        append(&queue, index_id, index_id).await.unwrap();
    }
    assert_eq!(retained_bytes(&queue), total_bytes);
    assert_eq!(queue.finish().await.unwrap().len(), 2);
    worker.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_upload_retry_preserves_bytes_and_waits_before_registration() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (core, file) = ingester_with_finished_file(dir.path());
    let (storage, mut uploads) = capture_uploads(vec![Err(StorageErrorKind::Timeout), Ok(())]);
    let (registered, mut registrations) = mpsc::unbounded_channel();
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .once()
        .returning(move |request| {
            registered.send(()).unwrap();
            Ok(registration_response(request))
        });
    let (queue, worker) = start_writer(core, storage, mock);
    let first_upload = uploads.recv().await.unwrap();
    assert!(registrations.try_recv().is_err());
    assert!(uploads.try_recv().is_err());
    assert!(file.path.is_file());
    assert_eq!(retained_bytes(&queue), file.num_bytes);
    let mut drain = Box::pin(queue.drain());
    assert!(poll!(&mut drain).is_pending());
    advance(Duration::from_secs(2)).await;
    assert_eq!(uploads.recv().await.unwrap(), first_upload);
    registrations.recv().await.unwrap();
    complete(drain).await.unwrap();
    assert_eq!(retained_bytes(&queue), 0);
    worker.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_registration_retry_and_restart_preserve_identity() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (core, file) = ingester_with_finished_file(dir.path());
    let (storage, mut uploads) = capture_uploads(vec![Ok(()), Ok(())]);
    let (called, mut calls) = mpsc::unbounded_channel();
    let mut attempt = 0;
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .times(3)
        .returning(move |request| {
            attempt += 1;
            called.send(request.clone()).unwrap();
            match attempt {
                1 => Err(MetastoreError::Unavailable(
                    "registration response lost".into(),
                )),
                2 => Err(MetastoreError::TooManyRequests),
                _ => Ok(registration_response(request)),
            }
        });
    let metastore = MetastoreServiceClient::from_mock(mock);
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let worker = tokio::spawn(write_batches(
        core,
        receiver,
        queue.admission.clone(),
        Some(Uploader::new(storage.clone(), metastore.clone())),
    ));
    let first_upload = uploads.recv().await.unwrap();
    let first_request = calls.recv().await.unwrap();
    assert!(file.path.is_file());
    assert_eq!(retained_bytes(&queue), file.num_bytes);
    assert!(calls.try_recv().is_err());
    advance(Duration::from_secs(2)).await;
    assert_eq!(calls.recv().await.unwrap(), first_request);
    assert!(uploads.try_recv().is_err()); // Known upload success survives RPC retries.
    assert_eq!(retained_bytes(&queue), file.num_bytes);
    let retained = queue.finish().await.unwrap();
    assert_eq!(retained[0].file_id, file.file_id);
    worker.await.unwrap().unwrap();

    // Restart loses upload progress, so it must replay the same object and request.
    let reopened = open_queue(dir.path(), Some(Uploader::new(storage, metastore)))
        .await
        .unwrap();
    assert_eq!(uploads.recv().await.unwrap(), first_upload);
    assert_eq!(calls.recv().await.unwrap(), first_request);
    assert_eq!(retained_bytes(&reopened), 0);
    assert!(!file.path.try_exists().unwrap());
    assert!(reopened.finish().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn test_deletion_failure_retains_charge_and_retries_only_deletion() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (core, file) = ingester_with_finished_file(dir.path());
    let (storage, mut uploads) = capture_uploads(vec![Ok(())]);
    let path = file.path.clone();
    let backup = dir.path().join("retained");
    let backup_for_mock = backup.clone();
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .once()
        .returning(move |request| {
            // A directory makes remove_file fail even when the test runs as root.
            fs::rename(&path, &backup_for_mock).unwrap();
            fs::create_dir(&path).unwrap();
            Ok(registration_response(request))
        });
    let (queue, worker) = start_writer(core, storage, mock);
    uploads.recv().await.unwrap();
    // Processing this queued request is a barrier after the prioritized deletion.
    let mut malformed = DocBatchV2::for_test([DOCUMENT]);
    malformed.doc_lengths[0] += 1;
    assert!(matches!(
        queue.append(file.index_uid.clone(), malformed, None).await,
        Err(IngestError::InvalidRequest(_))
    ));
    assert_eq!(retained_bytes(&queue), file.num_bytes);
    let mut drain = Box::pin(queue.drain());
    assert!(poll!(&mut drain).is_pending());
    fs::remove_dir(&file.path).unwrap();
    fs::rename(backup, &file.path).unwrap();
    advance(Duration::from_secs(2)).await;
    complete(drain).await.unwrap();
    assert_eq!(retained_bytes(&queue), 0);
    assert!(!file.path.try_exists().unwrap());
    // Exact mock call counts forbid repeating upload or registration during cleanup.
    worker.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_startup_finishes_recovered_handoff_before_intake_or_retains_files_on_cancellation() {
    let _clock = keep_clock_fixed();
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut core, file) = ingester_with_finished_file(dir.path());
        core.append(&file.index_uid, &[Bytes::from_static(DOCUMENT.as_bytes())])
            .unwrap();
        let mut recovered = core.finish().unwrap();
        recovered.sort_by_key(|file| file.file_id);
        let expected: Vec<_> = recovered
            .iter()
            .map(|file| fs::read(&file.path).unwrap())
            .collect();
        let (storage, mut uploads) = capture_uploads(vec![Ok(()); if cancel { 1 } else { 2 }]);
        let mut mock = MockMetastoreService::new();
        mock.expect_register_file_for_indexing()
            .times(if cancel { 0 } else { 2 })
            .returning(|request| Ok(registration_response(request)));
        let (metastore, permits) = stalled_metastore(mock);
        let mut opening = Box::pin(open_queue(
            dir.path(),
            Some(Uploader::new(storage, metastore)),
        ));
        complete(async {
            tokio::select! {
                result = &mut opening => panic!("startup passed stalled registration: {:?}", result.err()),
                upload = uploads.recv() => assert_eq!(upload.unwrap(), (PathBuf::from(recovered[0].file_id.to_string()), expected[0].clone())),
            }
        }).await;
        assert!(poll!(&mut opening).is_pending());
        assert!(uploads.try_recv().is_err());
        assert!(recovered.iter().all(|file| file.path.is_file()));
        if cancel {
            drop(opening);
            let core = reopen(dir.path()).await;
            assert_eq!(core.finished_files().len(), recovered.len());
            assert_eq!(
                core.disk_bytes,
                recovered.iter().map(|file| file.num_bytes).sum::<u64>()
            );
            for (file, bytes) in recovered.iter().zip(expected) {
                assert_eq!(fs::read(&file.path).unwrap(), bytes);
            }
            continue;
        }
        permits.add_permits(1);
        let queue = complete(opening).await.unwrap();
        assert_eq!(
            uploads.try_recv().unwrap(),
            (
                PathBuf::from(recovered[1].file_id.to_string()),
                expected[1].clone()
            )
        );
        assert_eq!(retained_bytes(&queue), 0);
        assert!(
            recovered
                .iter()
                .all(|file| !file.path.try_exists().unwrap())
        );
        let fresh_id = append(&queue, "logs", DOCUMENT)
            .await
            .unwrap()
            .file_id
            .unwrap();
        assert!(recovered.iter().all(|file| file.file_id != fresh_id));
        assert_eq!(queue.finish().await.unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn test_startup_cancellation_and_permanent_errors_release_directory_and_retain_data() {
    let _clock = keep_clock_fixed();
    for upload_error in [
        Some(StorageErrorKind::Timeout),
        Some(StorageErrorKind::Unauthorized),
        Some(StorageErrorKind::NotFound),
        None,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (core, file) = ingester_with_finished_file(dir.path());
        drop(core);
        let bytes = fs::read(&file.path).unwrap();
        let (storage, mut uploads) = capture_uploads(vec![upload_error.map(Err).unwrap_or(Ok(()))]);
        let mut mock = MockMetastoreService::new();
        mock.expect_register_file_for_indexing()
            .times(usize::from(upload_error.is_none()))
            .returning(|_| {
                Err(MetastoreError::NotFound(EntityKind::Source {
                    index_id: "logs".into(),
                    source_id: "unexpected-source".into(),
                }))
            });
        let uploader = Uploader::new(storage, MetastoreServiceClient::from_mock(mock));
        let mut opening = Box::pin(open_queue(dir.path(), Some(uploader)));
        if upload_error == Some(StorageErrorKind::Timeout) {
            complete(async {
                tokio::select! {
                    result = &mut opening => panic!("open returned before recovery upload succeeded: {:?}", result.err()),
                    attempt = uploads.recv() => assert_eq!(attempt.unwrap().1, bytes),
                }
            }).await;
            assert!(poll!(&mut opening).is_pending());
        } else {
            assert!(matches!(
                complete(&mut opening).await,
                Err(IngestError::Internal(_))
            ));
        }
        drop(opening);
        let recovered = reopen(dir.path()).await;
        assert_eq!(recovered.finished_files().len(), 1);
        assert_eq!(recovered.finished_files()[0].file_id, file.file_id);
        assert_eq!(recovered.disk_bytes, file.num_bytes);
        assert_eq!(fs::read(&file.path).unwrap(), bytes);
    }
}

#[tokio::test(start_paused = true)]
async fn test_drain_closes_clones_and_finishes_queued_and_active_files() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (mut core, file) = ingester_with_finished_file(dir.path());
    let index_uid = IndexUid::for_test("traces", 1);
    let active_id = core
        .append(&index_uid, &[Bytes::from_static(DOCUMENT.as_bytes())])
        .unwrap();
    let mut expected = fs::read(&core.active[&index_uid].input.path).unwrap();
    super::super::documents_staged_for_upload::write_record(&mut expected, b"queued").unwrap();
    let (storage, mut uploads) = capture_uploads(vec![Ok(()), Ok(())]);
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .times(2)
        .returning(|request| Ok(registration_response(request)));
    let (metastore, permits) = stalled_metastore(mock);
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let worker = tokio::spawn(write_batches(
        core,
        receiver,
        queue.admission.clone(),
        Some(Uploader::new(storage, metastore)),
    ));
    assert_eq!(
        complete(uploads.recv()).await.unwrap().1,
        fs::read(&file.path).unwrap()
    );
    // Poll admission without yielding to the writer, so this request is still queued at drain.
    let clone = queue.clone();
    let mut append = Box::pin(clone.append(index_uid, DocBatchV2::for_test(["queued"]), None));
    assert!(poll!(&mut append).is_pending());
    let mut drain = Box::pin(queue.drain());
    assert!(poll!(&mut drain).is_pending());
    assert_closed(&clone).await;
    assert_closed(&queue).await;
    assert_eq!(complete(append).await.unwrap().file_id, Some(active_id));
    assert!(poll!(&mut drain).is_pending());
    assert!(file.path.is_file());
    permits.add_permits(1);
    complete(drain).await.unwrap();
    // Acknowledging drain must release ownership before the caller's next await.
    assert!(
        Ingester::open(dir.path(), BUDGET)
            .unwrap()
            .finished_files()
            .is_empty()
    );
    worker.await.unwrap().unwrap();
    assert_eq!(
        uploads.try_recv().unwrap(),
        (PathBuf::from(active_id.to_string()), expected)
    );
    assert_eq!(retained_bytes(&queue), 0);
}

#[tokio::test(start_paused = true)]
async fn test_append_failure_cancels_registration_before_releasing_directory() {
    use quickwit_common::tower::BoxService;

    // Observe the actual directory lock while the stalled registration is dropped.
    struct CheckLockOnCancellation(PathBuf);
    impl Drop for CheckLockOnCancellation {
        fn drop(&mut self) {
            assert!(
                Ingester::open(&self.0, BUDGET).is_err(),
                "directory released before handoff cancellation"
            );
        }
    }

    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (mut core, file) = ingester_with_finished_file(dir.path());
    core.append(&file.index_uid, &[Bytes::from_static(b"accepted")])
        .unwrap();
    let active = core.active.get_mut(&file.index_uid).unwrap();
    active.writer = std::io::BufWriter::new(fs::File::open(&active.input.path).unwrap());
    let (storage, _uploads) = capture_uploads(vec![Ok(())]);
    let path = dir.path().to_path_buf();
    let (started, mut registrations) = mpsc::unbounded_channel();
    let metastore = MetastoreServiceClient::tower()
        .stack_register_file_for_indexing_layer(tower::layer::layer_fn(
            move |_: BoxService<
                RegisterFileForIndexing,
                RegisterFileForIndexingResponse,
                MetastoreError,
            >| {
                let path = path.clone();
                let started = started.clone();
                tower::service_fn(move |_| {
                    let check = CheckLockOnCancellation(path.clone());
                    let started = started.clone();
                    async move {
                        let _check = check;
                        started.send(()).unwrap();
                        std::future::pending::<
                            Result<RegisterFileForIndexingResponse, MetastoreError>,
                        >()
                        .await
                    }
                })
            },
        ))
        .build_from_mock(MockMetastoreService::new());
    let (queue, receiver) = WriteQueue::channel(&core, BUDGET);
    let worker = tokio::spawn(write_batches(
        core,
        receiver,
        queue.admission.clone(),
        Some(Uploader::new(storage, metastore)),
    ));
    complete(registrations.recv()).await.unwrap();
    assert!(matches!(
        append(&queue, "logs", DOCUMENT).await,
        Err(IngestError::Internal(_))
    ));
    assert!(complete(worker).await.unwrap().is_err());
    assert!(Ingester::open(dir.path(), BUDGET).is_ok());
}

#[tokio::test(start_paused = true)]
async fn test_deleted_index_is_discarded_and_other_indexes_continue() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (mut core, file) = ingester_with_finished_file(dir.path());
    let active_id = core
        .append(
            &IndexUid::for_test("traces", 1),
            &[Bytes::from_static(b"accepted")],
        )
        .unwrap();
    let retained = core.disk_bytes - file.num_bytes;
    let (mut storage, mut uploads) = capture_uploads(vec![Ok(()), Ok(()), Ok(())]);
    let deleted_key = PathBuf::from(file.file_id.to_string());
    Arc::get_mut(&mut storage)
        .unwrap()
        .expect_delete()
        .once()
        .withf(move |key| key == deleted_key)
        .returning(|_| Ok(()));
    let mut mock = MockMetastoreService::new();
    let old_uid = file.index_uid.clone();
    mock.expect_register_file_for_indexing()
        .times(3)
        .returning(move |request| {
            if request.index_uid() == &old_uid {
                return Err(MetastoreError::NotFound(EntityKind::Index {
                    index_id: "logs".into(),
                }));
            }
            Ok(registration_response(request))
        });
    let (queue, worker) = start_writer(core, storage, mock);
    complete(async {
        while retained_bytes(&queue) != retained {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(!file.path.try_exists().unwrap());
    assert!(!worker.is_finished());
    assert_eq!(
        append(&queue, "traces", DOCUMENT).await.unwrap().file_id,
        Some(active_id)
    );
    // Recreating the name never redirects the old incarnation's documents.
    let new_id = queue
        .append(
            IndexUid::for_test("logs", 2),
            DocBatchV2::for_test(["new incarnation"]),
            None,
        )
        .await
        .unwrap()
        .file_id
        .unwrap();
    assert_ne!(new_id, file.file_id);
    complete(queue.drain()).await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(retained_bytes(&queue), 0);
    let mut uploaded_ids = HashSet::new();
    while let Ok((key, _)) = uploads.try_recv() {
        uploaded_ids.insert(key);
    }
    assert_eq!(
        uploaded_ids,
        HashSet::from([
            PathBuf::from(file.file_id.to_string()),
            PathBuf::from(active_id.to_string()),
            PathBuf::from(new_id.to_string()),
        ])
    );
    assert!(
        Ingester::open(dir.path(), BUDGET)
            .unwrap()
            .finished_files()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn test_deleted_index_cleanup_during_startup_and_drain_retries_only_deletion() {
    let _clock = keep_clock_fixed();
    for recovering in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        if recovering {
            let (core, _) = ingester_with_finished_file(dir.path());
            drop(core);
        }
        let (mut storage, mut uploads) = capture_uploads(vec![Ok(())]);
        let (deleted, mut deletions) = mpsc::unbounded_channel();
        let mut outcomes = [StorageErrorKind::Timeout, StorageErrorKind::NotFound].into_iter();
        Arc::get_mut(&mut storage)
            .unwrap()
            .expect_delete()
            .times(2)
            .returning(move |key| {
                deleted.send(key.to_path_buf()).unwrap();
                Err(outcomes
                    .next()
                    .unwrap()
                    .with_error(anyhow::anyhow!("delete response")))
            });
        let mut mock = MockMetastoreService::new();
        mock.expect_register_file_for_indexing()
            .once()
            .returning(|_| {
                Err(MetastoreError::NotFound(EntityKind::Index {
                    index_id: "logs".into(),
                }))
            });
        let uploader = Uploader::new(storage, MetastoreServiceClient::from_mock(mock));
        let mut operation = Box::pin(async {
            let queue = open_queue(dir.path(), Some(uploader)).await.unwrap();
            if !recovering {
                append(&queue, "logs", DOCUMENT).await.unwrap();
                queue.drain().await.unwrap();
            }
            queue
        });
        let key = complete(async {
            tokio::select! {
                _ = &mut operation => panic!("cleanup must wait for deletion"),
                key = deletions.recv() => key.unwrap(),
            }
        })
        .await;
        assert_eq!(uploads.try_recv().unwrap().0, key);
        let local_path = dir
            .path()
            .join("ingesthree")
            .join(&key)
            .with_extension("finished");
        assert!(local_path.try_exists().unwrap());
        assert!(poll!(&mut operation).is_pending());
        advance(Duration::from_secs(1)).await;
        let queue = complete(operation).await;
        assert_eq!(deletions.try_recv().unwrap(), key);
        assert!(!local_path.try_exists().unwrap());
        assert_eq!(retained_bytes(&queue), 0);
        if recovering {
            // Startup opened intake after discarding the obsolete recovered file.
            append(&queue, "traces", DOCUMENT).await.unwrap();
            assert_eq!(queue.finish().await.unwrap().len(), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_drain_without_uploader_or_with_permanent_registration_error_retains_data() {
    let _clock = keep_clock_fixed();
    for error in [
        None,
        Some(MetastoreError::NotFound(EntityKind::Source {
            index_id: "logs".into(),
            source_id: "unexpected-source".into(),
        })),
        Some(MetastoreError::Forbidden {
            message: "forbidden".into(),
        }),
        Some(MetastoreError::InvalidArgument {
            message: "invalid registration".into(),
        }),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (storage, _uploads) = capture_uploads(if error.is_some() {
            vec![Ok(())]
        } else {
            vec![]
        });
        let uploader = error.map(|error| {
            let mut mock = MockMetastoreService::new();
            mock.expect_register_file_for_indexing()
                .once()
                .returning(move |_| Err(error.clone()));
            Uploader::new(storage, MetastoreServiceClient::from_mock(mock))
        });
        let queue = open_queue(dir.path(), uploader).await.unwrap();
        let file_id = append(&queue, "logs", DOCUMENT)
            .await
            .unwrap()
            .file_id
            .unwrap();
        let path = dir
            .path()
            .join("ingesthree")
            .join(format!("{file_id}.open"));
        let bytes = fs::read(&path).unwrap();
        assert!(complete(queue.drain()).await.is_err());
        let recovered = Ingester::open(dir.path(), BUDGET).unwrap();
        assert_closed(&queue.clone()).await;
        assert_eq!(retained_bytes(&queue), bytes.len() as u64);
        assert_eq!(recovered.finished_files().len(), 1);
        assert_eq!(recovered.finished_files()[0].file_id, file_id);
        assert_eq!(fs::read(path.with_extension("finished")).unwrap(), bytes);
    }
}
