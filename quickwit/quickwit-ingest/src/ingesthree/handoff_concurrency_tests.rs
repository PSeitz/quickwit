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

use super::*;

#[tokio::test(start_paused = true)]
async fn test_ready_file_uploads_while_another_upload_waits_to_retry() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (mut core, first) = ingester_with_finished_file(dir.path());
    core.file_limits.target_file_bytes = 1;
    let (storage, mut uploads) =
        capture_uploads(vec![Err(StorageErrorKind::Timeout), Ok(()), Ok(())]);
    let (registered, mut registrations) = mpsc::unbounded_channel();
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .times(2)
        .returning(move |request| {
            registered.send(request.file_id.clone()).unwrap();
            Ok(registration_response(request))
        });
    let (queue, worker) = start_writer(core, storage, mock);
    let first_upload = complete(uploads.recv()).await.unwrap();
    assert_eq!(first_upload.0, PathBuf::from(first.file_id.to_string()));

    let second_id = complete(append(&queue, "traces", DOCUMENT))
        .await
        .unwrap()
        .file_id
        .unwrap();
    // Keep time frozen: the first file cannot finish its retry delay. The newly
    // finished file must upload, register, and release its capacity independently.
    assert_eq!(
        complete(uploads.recv()).await.unwrap().0,
        PathBuf::from(second_id.to_string())
    );
    assert_eq!(
        complete(registrations.recv()).await.unwrap(),
        second_id.to_string()
    );
    complete(async {
        while retained_bytes(&queue) != first.num_bytes {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(first.path.is_file());
    assert!(
        !first
            .path
            .with_file_name(format!("{second_id}.finished"))
            .try_exists()
            .unwrap()
    );
    assert!(uploads.try_recv().is_err());

    advance(Duration::from_secs(2)).await;
    assert_eq!(complete(uploads.recv()).await.unwrap(), first_upload);
    assert_eq!(
        complete(registrations.recv()).await.unwrap(),
        first.file_id.to_string()
    );
    complete(queue.drain()).await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(retained_bytes(&queue), 0);
}

#[tokio::test(start_paused = true)]
async fn test_local_deletion_retry_does_not_delay_other_files() {
    let _clock = keep_clock_fixed();
    let dir = tempfile::tempdir().unwrap();
    let (mut core, first) = ingester_with_finished_file(dir.path());
    core.file_limits.target_file_bytes = 1;
    let (storage, mut uploads) = capture_uploads(vec![Ok(()), Ok(())]);
    let path = first.path.clone();
    let backup = dir.path().join("retained");
    let backup_for_mock = backup.clone();
    let first_id = first.file_id.to_string();
    let mut mock = MockMetastoreService::new();
    mock.expect_register_file_for_indexing()
        .times(2)
        .returning(move |request| {
            if request.file_id == first_id {
                fs::rename(&path, &backup_for_mock).unwrap();
                fs::create_dir(&path).unwrap();
            }
            Ok(registration_response(request))
        });
    let (queue, worker) = start_writer(core, storage, mock);
    complete(uploads.recv()).await.unwrap();
    let second_id = complete(append(&queue, "traces", DOCUMENT))
        .await
        .unwrap()
        .file_id
        .unwrap();
    assert_eq!(
        complete(uploads.recv()).await.unwrap().0,
        PathBuf::from(second_id.to_string())
    );
    complete(async {
        while retained_bytes(&queue) != first.num_bytes {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(first.path.is_dir());
    assert!(
        !first
            .path
            .with_file_name(format!("{second_id}.finished"))
            .try_exists()
            .unwrap()
    );

    fs::remove_dir(&first.path).unwrap();
    fs::rename(backup, &first.path).unwrap();
    advance(Duration::from_secs(2)).await;
    complete(queue.drain()).await.unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(retained_bytes(&queue), 0);
    assert!(uploads.try_recv().is_err());
}
