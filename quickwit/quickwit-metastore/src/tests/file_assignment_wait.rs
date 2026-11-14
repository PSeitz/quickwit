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

use std::collections::HashSet;

use quickwit_proto::metastore::{AssignFilesForIndexing, MetastoreService};
use time::OffsetDateTime;

use super::DefaultForTest;
use super::staged_file::{
    WriteStagedFileForTest, assignment_request, create_index, delete_index, publication_request,
    read_file, register_files,
};

pub async fn test_assign_files_wait_for_prefix<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let mut files = register_files(&metastore, &index_uid, &[40, 20, 100]).await;
    // Keep the age gate closed independently of time spent in database calls.
    let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    for (i, file) in files.iter_mut().enumerate() {
        file.create_timestamp = future + i as i64;
        metastore.set_staged_file(file.clone()).await;
    }
    let request = AssignFilesForIndexing {
        max_num_files: 2,
        max_num_bytes: 100,
        max_wait_secs: 15,
        ..assignment_request()
    };
    // The count-capped prefix has only 60 bytes, even though the index has 160.
    assert!(
        metastore
            .assign_files_for_indexing(request)
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    for file in &files {
        assert_eq!(read_file(&metastore, file).await, *file);
    }
    files[0].create_timestamp = OffsetDateTime::now_utc().unix_timestamp() - 15;
    metastore.set_staged_file(files[0].clone()).await;
    let assigned = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 2);
    assert_eq!(assigned[0].file_id, files[0].file_id);
    assert_eq!(assigned[1].file_id, files[1].file_id);
    assert_eq!(read_file(&metastore, &files[2]).await, files[2]);
    delete_index(&mut metastore, index_uid).await;

    let index_uid = create_index(&mut metastore).await;
    let mut files = register_files(&metastore, &index_uid, &[40, 70, 10]).await;
    for (i, file) in files.iter_mut().enumerate() {
        file.create_timestamp = future + i as i64;
        metastore.set_staged_file(file.clone()).await;
    }
    let assigned = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 2);
    assert_eq!(assigned[0].file_id, files[0].file_id);
    assert_eq!(assigned[1].file_id, files[1].file_id);
    assert_eq!(assigned.iter().map(|file| file.num_bytes).sum::<u64>(), 110);
    assert_eq!(read_file(&metastore, &files[2]).await, files[2]);
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_assign_files_wait_skips_underfilled_indexes<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let waiting_index = create_index(&mut metastore).await;
    let ready_index = create_index(&mut metastore).await;
    let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    let mut waiting = register_files(&metastore, &waiting_index, &[10]).await;
    waiting[0].create_timestamp = future;
    metastore.set_staged_file(waiting[0].clone()).await;
    let mut ready = register_files(&metastore, &ready_index, &[60, 50]).await;
    for (i, file) in ready.iter_mut().enumerate() {
        file.create_timestamp = future + 1 + i as i64;
        metastore.set_staged_file(file.clone()).await;
    }
    let request = AssignFilesForIndexing {
        max_num_bytes: 100,
        max_wait_secs: 15,
        ..assignment_request()
    };
    let assigned = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 2);
    assert!(assigned.iter().all(|file| file.index_uid() == &ready_index));
    assert_eq!(read_file(&metastore, &waiting[0]).await, waiting[0]);
    assert!(
        metastore
            .assign_files_for_indexing(request)
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    delete_index(&mut metastore, waiting_index).await;
    delete_index(&mut metastore, ready_index).await;
}

pub async fn test_assign_files_wait_excludes_completed_and_inflight<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[1000, 1000]).await;
    metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap();
    metastore
        .publish_file_splits(publication_request(&index_uid, &files[..1], &[]))
        .await
        .unwrap();
    let mut excluded = Vec::new();
    for file in &files {
        let mut stored = read_file(&metastore, file).await;
        stored.create_timestamp = 1;
        metastore.set_staged_file(stored.clone()).await;
        excluded.push(stored);
    }
    let mut fresh = register_files(&metastore, &index_uid, &[40]).await;
    fresh[0].create_timestamp = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    metastore.set_staged_file(fresh[0].clone()).await;
    let request = AssignFilesForIndexing {
        max_num_bytes: 100,
        max_wait_secs: 15,
        ..assignment_request()
    };
    assert!(
        metastore
            .assign_files_for_indexing(request)
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    for file in excluded.iter().chain(&fresh) {
        assert_eq!(read_file(&metastore, file).await, *file);
    }
    // Retry eligibility is unchanged. An expired attempt retains its registration age.
    excluded[1].processing_started_timestamp = Some(1);
    metastore.set_staged_file(excluded[1].clone()).await;
    let assigned = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].file_id, excluded[1].file_id);
    assert_eq!(read_file(&metastore, &excluded[0]).await, excluded[0]);
    assert_eq!(read_file(&metastore, &fresh[0]).await, fresh[0]);
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_assign_files_wait_concurrent<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let mut files = register_files(&metastore, &index_uid, &[60; 8]).await;
    for file in &mut files {
        file.create_timestamp = OffsetDateTime::now_utc().unix_timestamp() + 3600;
        metastore.set_staged_file(file.clone()).await;
    }
    let request = AssignFilesForIndexing {
        max_num_files: 2,
        max_num_bytes: 100,
        max_wait_secs: 15,
        ..assignment_request()
    };
    let replies =
        futures::future::join_all((0..8).map(|_| metastore.assign_files_for_indexing(request)))
            .await;
    let mut assigned_ids = HashSet::new();
    for reply in replies {
        let assigned = reply.unwrap().staged_files;
        assert!(assigned.is_empty() || assigned.len() == 2);
        for file in assigned {
            assert!(assigned_ids.insert(file.file_id.clone()));
            assert!(file.processing_started_timestamp.is_some());
            assert_eq!(read_file(&metastore, &file).await, file);
        }
    }
    // Lock contention may defer a ready prefix to a later poll.
    loop {
        let assigned = metastore
            .assign_files_for_indexing(request)
            .await
            .unwrap()
            .staged_files;
        if assigned.is_empty() {
            break;
        }
        assert_eq!(assigned.len(), 2);
        for file in assigned {
            assert!(assigned_ids.insert(file.file_id));
        }
    }
    assert_eq!(
        assigned_ids,
        files.iter().map(|file| file.file_id.clone()).collect()
    );
    delete_index(&mut metastore, index_uid).await;
}
