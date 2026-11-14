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

use quickwit_proto::metastore::{AssignFilesForIndexing, MetastoreService};
use sqlx::Acquire;
use time::OffsetDateTime;
use tokio::time::timeout;

use super::PostgresqlMetastore;
use crate::tests::DefaultForTest;
use crate::tests::staged_file::{
    WriteStagedFileForTest, assignment_request, create_index, delete_index, read_file,
    register_files,
};

#[tokio::test]
#[serial_test::file_serial]
async fn test_assignment_locked_bytes_do_not_hide_another_ready_index() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let first_index = create_index(&mut metastore).await;
    let second_index = create_index(&mut metastore).await;
    let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    let mut first = register_files(&metastore, &first_index, &[60, 60]).await;
    for (i, file) in first.iter_mut().enumerate() {
        file.create_timestamp = future + i as i64;
        metastore.set_staged_file(file.clone()).await;
    }
    let mut second = register_files(&metastore, &second_index, &[100]).await;
    second[0].create_timestamp = future + 2;
    metastore.set_staged_file(second[0].clone()).await;
    let request = AssignFilesForIndexing {
        max_num_bytes: 100,
        max_wait_secs: 15,
        ..assignment_request()
    };
    let mut tx = metastore.connection_pool.begin().await.unwrap();
    sqlx::query("SELECT file_id FROM staged_files WHERE file_id = $1 FOR UPDATE")
        .bind(&first[0].file_id)
        .execute(tx.as_mut())
        .await
        .unwrap();
    // The index has 120 bytes, but only its unlocked 60-byte file can be claimed.
    let assigned = timeout(
        Duration::from_secs(5),
        metastore.assign_files_for_indexing(request),
    )
    .await
    .expect("assignment must skip locked files and continue to another ready index")
    .unwrap()
    .staged_files;
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].file_id, second[0].file_id);
    assert_eq!(read_file(&metastore, &first[1]).await, first[1]);
    assert!(
        timeout(
            Duration::from_secs(5),
            metastore.assign_files_for_indexing(request)
        )
        .await
        .unwrap()
        .unwrap()
        .staged_files
        .is_empty()
    );
    // A rejected candidate releases its row locks before returning.
    timeout(
        Duration::from_secs(5),
        sqlx::query("SELECT file_id FROM staged_files WHERE file_id = $1 FOR UPDATE NOWAIT")
            .bind(&first[1].file_id)
            .execute(tx.as_mut()),
    )
    .await
    .unwrap()
    .unwrap();
    tx.rollback().await.unwrap();
    let assigned = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 2);
    assert_eq!(assigned[0].file_id, first[0].file_id);
    assert_eq!(assigned[1].file_id, first[1].file_id);
    delete_index(&mut metastore, first_index).await;
    delete_index(&mut metastore, second_index).await;
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_assignment_count_cap_excludes_locked_files() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let mut files = register_files(&metastore, &index_uid, &[10, 10, 100]).await;
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
    let mut tx = metastore.connection_pool.begin().await.unwrap();
    sqlx::query("SELECT file_id FROM staged_files WHERE file_id = ANY($1) FOR UPDATE")
        .bind(vec![files[0].file_id.clone(), files[1].file_id.clone()])
        .execute(tx.as_mut())
        .await
        .unwrap();
    let assigned = timeout(
        Duration::from_secs(5),
        metastore.assign_files_for_indexing(request),
    )
    .await
    .unwrap()
    .unwrap()
    .staged_files;
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].file_id, files[2].file_id);
    assert_eq!(read_file(&metastore, &files[0]).await, files[0]);
    assert_eq!(read_file(&metastore, &files[1]).await, files[1]);
    tx.rollback().await.unwrap();
    delete_index(&mut metastore, index_uid).await;
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_assignment_locked_oldest_does_not_release_fresh_files() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let mut files = register_files(&metastore, &index_uid, &[40, 40]).await;
    files[0].create_timestamp = 1;
    files[1].create_timestamp = OffsetDateTime::now_utc().unix_timestamp() + 3600;
    for file in &files {
        metastore.set_staged_file(file.clone()).await;
    }
    let request = AssignFilesForIndexing {
        max_num_bytes: 100,
        max_wait_secs: 15,
        ..assignment_request()
    };
    let mut tx = metastore.connection_pool.begin().await.unwrap();
    sqlx::query("SELECT file_id FROM staged_files WHERE file_id = $1 FOR UPDATE")
        .bind(&files[0].file_id)
        .execute(tx.as_mut())
        .await
        .unwrap();
    assert!(
        timeout(
            Duration::from_secs(5),
            metastore.assign_files_for_indexing(request)
        )
        .await
        .unwrap()
        .unwrap()
        .staged_files
        .is_empty()
    );
    assert_eq!(read_file(&metastore, &files[1]).await, files[1]);
    tx.rollback().await.unwrap();
    let assigned = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 2);
    assert_eq!(assigned[0].file_id, files[0].file_id);
    assert_eq!(assigned[1].file_id, files[1].file_id);
    delete_index(&mut metastore, index_uid).await;
}
