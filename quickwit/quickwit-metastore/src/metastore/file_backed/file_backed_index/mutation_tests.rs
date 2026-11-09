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

use std::sync::{Arc, mpsc};
use std::time::Duration;

use futures::executor::block_on;
use quickwit_proto::metastore::{DeleteIndexRequest, MetastoreService, StageSplitsRequest};
use quickwit_proto::types::IndexUid;
use quickwit_storage::{MockStorage, RamStorage, Storage};
use tokio::time::timeout;
use ulid::Ulid;

use crate::file_backed::FileBackedMetastore;
use crate::file_backed::store_operations::{load_index, metastore_filepath};
use crate::tests::staged_file::{
    assignment_request, create_index, publication_request, register_files, registration_request,
};
use crate::{SplitMetadata, SplitState, StageSplitsRequestExt};

const WAIT: Duration = Duration::from_secs(10);

/// Gate one metadata PUT before persistence or after it commits but before its response.
/// MockStorage's async methods have synchronous callbacks, so these tests use several Tokio
/// workers and bounded channel waits. All other storage operations still reach real storage.
fn gate_next_put(
    metastore: &mut FileBackedMetastore,
    storage: Arc<RamStorage>,
    index_uid: &IndexUid,
    persist_first: bool,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let mut gate = Some((ready_tx, resume_rx));
    let metadata_path = metastore_filepath(&index_uid.index_id);
    let mut mock = MockStorage::default();
    let read_storage = storage.clone();
    mock.expect_get_all()
        .returning(move |path| block_on(read_storage.get_all(path)));
    let exists_storage = storage.clone();
    mock.expect_exists()
        .returning(move |path| block_on(exists_storage.exists(path)));
    let delete_storage = storage.clone();
    mock.expect_delete()
        .returning(move |path| block_on(delete_storage.delete(path)));
    mock.expect_put().returning(move |path, payload| {
        let gate = if path == metadata_path {
            gate.take()
        } else {
            None
        };
        let Some((ready, resume)) = gate else {
            return block_on(storage.put(path, payload));
        };
        if persist_first {
            block_on(storage.put(path, payload))?;
            ready.send(()).unwrap();
            resume.recv_timeout(WAIT).unwrap();
            Ok(())
        } else {
            ready.send(()).unwrap();
            resume.recv_timeout(WAIT).unwrap();
            block_on(storage.put(path, payload))
        }
    });
    metastore.storage = Arc::new(mock);
    (ready_rx, resume_tx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cancelled_publication_keeps_committed_winner() {
    let storage = Arc::new(RamStorage::default());
    let mut metastore = FileBackedMetastore::try_new(storage.clone(), None)
        .await
        .unwrap();
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100]).await;
    for split_id in ["winner", "loser"] {
        metastore
            .stage_splits(
                StageSplitsRequest::try_from_split_metadata(
                    index_uid.clone(),
                    &SplitMetadata::for_test(split_id.into()),
                )
                .unwrap(),
            )
            .await
            .unwrap();
    }
    let winner = publication_request(&index_uid, &files, &["winner"]);
    let loser = publication_request(&index_uid, &files, &["loser"]);
    let (persisted, resume) = gate_next_put(&mut metastore, storage.clone(), &index_uid, true);
    let publisher = metastore.clone();
    let request = winner.clone();
    let publishing = tokio::spawn(async move { publisher.publish_file_splits(request).await });
    persisted.recv_timeout(WAIT).unwrap();
    // The durable winner already exists while this same instance still has the old cache.
    let durable = load_index(&*storage, &index_uid.index_id).await.unwrap();
    assert_eq!(
        durable.staged_files[&files[0].file_id]
            .publication_id
            .as_deref(),
        Some(winner.publication_id.as_str())
    );
    publishing.abort();
    assert!(
        timeout(WAIT, publishing)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );

    let competing = metastore.publish_file_splits(loser);
    let retry = metastore.publish_file_splits(winner);
    tokio::pin!(competing, retry);
    assert!(futures::poll!(&mut competing).is_pending());
    assert!(futures::poll!(&mut retry).is_pending());
    resume.send(()).unwrap();
    assert!(!timeout(WAIT, competing).await.unwrap().unwrap().published);
    assert!(timeout(WAIT, retry).await.unwrap().unwrap().published);
    let index = metastore.get_index(&index_uid).await.unwrap();
    assert_eq!(index.splits["winner"].split_state, SplitState::Published);
    assert_eq!(index.splits["loser"].split_state, SplitState::Staged);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cancelled_assignment_keeps_committed_claim() {
    let storage = Arc::new(RamStorage::default());
    let mut metastore = FileBackedMetastore::try_new(storage.clone(), None)
        .await
        .unwrap();
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100]).await;
    let (persisted, resume) = gate_next_put(&mut metastore, storage.clone(), &index_uid, true);
    let assigner = metastore.clone();
    let assigning = tokio::spawn(async move {
        assigner
            .assign_files_for_indexing(assignment_request())
            .await
    });
    persisted.recv_timeout(WAIT).unwrap();
    let durable = load_index(&*storage, &index_uid.index_id).await.unwrap();
    assert!(
        durable.staged_files[&files[0].file_id]
            .processing_started_timestamp
            .is_some()
    );
    assigning.abort();
    assert!(
        timeout(WAIT, assigning)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
    let competing = metastore.assign_files_for_indexing(assignment_request());
    tokio::pin!(competing);
    assert!(futures::poll!(&mut competing).is_pending());
    resume.send(()).unwrap();
    assert!(
        timeout(WAIT, competing)
            .await
            .unwrap()
            .unwrap()
            .staged_files
            .is_empty()
    );
    let index = metastore.get_index(&index_uid).await.unwrap();
    assert_eq!(index.staged_files, durable.staged_files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cancelled_mutations_finish_before_deletion() {
    // Assignment holds the exclusive guard. Registration exercises ordinary shared mutations.
    for assign_files in [false, true] {
        for persist_first in [false, true] {
            let storage = Arc::new(RamStorage::default());
            let mut metastore = FileBackedMetastore::try_new(storage.clone(), None)
                .await
                .unwrap();
            let index_uid = create_index(&mut metastore).await;
            register_files(&metastore, &index_uid, &[100]).await;
            let (ready, resume) =
                gate_next_put(&mut metastore, storage.clone(), &index_uid, persist_first);
            let writer = metastore.clone();
            let writer_index_uid = index_uid.clone();
            let writing = tokio::spawn(async move {
                if assign_files {
                    writer
                        .assign_files_for_indexing(assignment_request())
                        .await
                        .map(|_| ())
                } else {
                    writer
                        .register_file_for_indexing(registration_request(writer_index_uid))
                        .await
                        .map(|_| ())
                }
            });
            ready.recv_timeout(WAIT).unwrap();
            writing.abort();
            assert!(
                timeout(WAIT, writing)
                    .await
                    .unwrap()
                    .unwrap_err()
                    .is_cancelled()
            );
            let deletion = metastore.delete_index(DeleteIndexRequest {
                index_uid: Some(index_uid.clone()),
            });
            tokio::pin!(deletion);
            assert!(futures::poll!(&mut deletion).is_pending());
            resume.send(()).unwrap();
            timeout(WAIT, deletion).await.unwrap().unwrap();
            // The outstanding PUT has completed before DELETE, including when it had not
            // reached storage at cancellation. Nothing can recreate the metadata afterwards.
            assert!(
                !storage
                    .exists(&metastore_filepath(&index_uid.index_id))
                    .await
                    .unwrap()
            );
            assert!(
                !metastore
                    .state
                    .read()
                    .await
                    .indexes
                    .contains_key(&index_uid.index_id)
            );
        }
    }
}

#[tokio::test]
async fn test_assignment_scan_excludes_concurrent_registration() {
    let mut metastore = FileBackedMetastore::try_new(Arc::new(RamStorage::default()), None)
        .await
        .unwrap();
    create_index(&mut metastore).await;
    create_index(&mut metastore).await;
    // Follow the scan's map iteration order. Hold the second index as a barrier after the
    // first empty index has been scanned, where an uncoordinated registration was missed.
    let index_ids: Vec<_> = metastore
        .state
        .read()
        .await
        .indexes
        .keys()
        .cloned()
        .collect();
    let first_uid = metastore
        .get_locked_index(&index_ids[0])
        .await
        .unwrap()
        .index_uid()
        .clone();
    let second = metastore.get_locked_index(&index_ids[1]).await.unwrap();
    let second_uid = second.index_uid().clone();
    let assigning = metastore.assign_files_for_indexing(assignment_request());
    tokio::pin!(assigning);
    assert!(futures::poll!(&mut assigning).is_pending());
    let mut older = registration_request(first_uid);
    older.file_id = Ulid::from(1u128).to_string();
    let mut newer = registration_request(second_uid);
    newer.file_id = Ulid::from(2u128).to_string();
    let register_older = metastore.register_file_for_indexing(older);
    let register_newer = metastore.register_file_for_indexing(newer);
    tokio::pin!(register_older, register_newer);
    assert!(futures::poll!(&mut register_older).is_pending());
    assert!(futures::poll!(&mut register_newer).is_pending());
    drop(second);
    assert!(
        timeout(WAIT, assigning)
            .await
            .unwrap()
            .unwrap()
            .staged_files
            .is_empty()
    );
    let older_file = register_older.await.unwrap().staged_file.unwrap();
    register_newer.await.unwrap();
    let assigned = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap();
    assert_eq!(assigned.staged_files.len(), 1);
    assert_eq!(assigned.staged_files[0].file_id, older_file.file_id);
}

#[tokio::test]
async fn test_ordinary_mutations_on_different_indexes_can_overlap() {
    let mut metastore = FileBackedMetastore::try_new(Arc::new(RamStorage::default()), None)
        .await
        .unwrap();
    let first_uid = create_index(&mut metastore).await;
    let second_uid = create_index(&mut metastore).await;
    let first_index = metastore
        .get_locked_index(&first_uid.index_id)
        .await
        .unwrap();
    let first = metastore.register_file_for_indexing(registration_request(first_uid));
    tokio::pin!(first);
    assert!(futures::poll!(&mut first).is_pending());
    timeout(
        WAIT,
        metastore.register_file_for_indexing(registration_request(second_uid)),
    )
    .await
    .unwrap()
    .unwrap();
    drop(first_index);
    timeout(WAIT, first).await.unwrap().unwrap();
}
