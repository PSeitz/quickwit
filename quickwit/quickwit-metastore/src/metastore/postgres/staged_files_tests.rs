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
use std::time::Duration;

use quickwit_proto::metastore::{
    AssignFilesForIndexing, DeleteSplitsRequest, DocumentsStagedForUpload, EntityKind,
    MarkSplitsForDeletionRequest, MetastoreError, MetastoreService, PublishFileSplits,
    RegisterFileForIndexing, StageSplitsRequest,
};
use quickwit_proto::types::IndexUid;
use sqlx::Acquire;
use tokio::time::timeout;
use ulid::Ulid;

use super::PostgresqlMetastore;
use crate::tests::DefaultForTest;
use crate::tests::file_publication::stage_splits;
use crate::tests::staged_file::{
    WriteStagedFileForTest, create_index, delete_index, registration_request,
};
use crate::{SplitMetadata, StageSplitsRequestExt};

async fn register_files(
    metastore: &PostgresqlMetastore,
    index_uid: &IndexUid,
    count: usize,
) -> Vec<RegisterFileForIndexing> {
    let mut requests = Vec::new();
    for i in 0..count {
        let request = registration_request(index_uid.clone());
        let mut file = metastore
            .register_file_for_indexing(request.clone())
            .await
            .unwrap()
            .staged_file
            .unwrap();
        file.create_timestamp = i as i64 + 1;
        metastore.set_staged_file(file).await;
        requests.push(request);
    }
    requests
}

async fn read_file(
    metastore: &PostgresqlMetastore,
    request: &RegisterFileForIndexing,
) -> DocumentsStagedForUpload {
    metastore
        .register_file_for_indexing(request.clone())
        .await
        .unwrap()
        .staged_file
        .unwrap()
}

async fn stage_output(metastore: &PostgresqlMetastore, index_uid: &IndexUid) -> String {
    let split_id = Ulid::new().to_string();
    let metadata = SplitMetadata {
        index_uid: index_uid.clone(),
        split_id: split_id.clone().into(),
        ..Default::default()
    };
    metastore
        .stage_splits(
            StageSplitsRequest::try_from_split_metadata(index_uid.clone(), &metadata).unwrap(),
        )
        .await
        .unwrap();
    split_id
}

async fn split_state(
    metastore: &PostgresqlMetastore,
    index_uid: &IndexUid,
    split_id: &str,
) -> String {
    sqlx::query_scalar("SELECT split_state FROM splits WHERE index_uid = $1 AND split_id = $2")
        .bind(index_uid)
        .bind(split_id)
        .fetch_one(&metastore.connection_pool)
        .await
        .unwrap()
}

async fn wait_for_blocked_query(metastore: &PostgresqlMetastore, blocking_pid: i32) {
    timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = \
                 ANY(pg_blocking_pids(pid)))",
            )
            .bind(blocking_pid)
            .fetch_one(&metastore.connection_pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("query must wait for the held lock");
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_registration_stores_whole_seconds() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let file = read_file(&metastore, &registration_request(index_uid.clone())).await;
    let timestamp: time::OffsetDateTime =
        sqlx::query_scalar("SELECT create_timestamp FROM staged_files WHERE file_id = $1")
            .bind(&file.file_id)
            .fetch_one(&metastore.connection_pool)
            .await
            .unwrap();
    assert_eq!(timestamp.nanosecond(), 0);
    assert_eq!(timestamp.unix_timestamp(), file.create_timestamp);
    delete_index(&mut metastore, index_uid).await;
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_assignment_skips_locked_files_and_concurrent_groups_are_disjoint() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let requests = register_files(&metastore, &index_uid, 9).await;
    let mut tx = metastore.connection_pool.begin().await.unwrap();
    sqlx::query("SELECT file_id FROM staged_files WHERE file_id = $1 FOR UPDATE")
        .bind(&requests[0].file_id)
        .execute(tx.as_mut())
        .await
        .unwrap();
    let assignment = AssignFilesForIndexing {
        max_num_files: 2,
        max_num_bytes: 1_000,
        retry_after_secs: 60,
    };
    let first = timeout(
        Duration::from_secs(5),
        metastore.assign_files_for_indexing(assignment),
    )
    .await
    .expect("assignment must skip the locked oldest file")
    .unwrap();
    assert_eq!(first.staged_files[0].file_id, requests[1].file_id);
    assert_eq!(first.staged_files[1].file_id, requests[2].file_id);
    let replies = timeout(
        Duration::from_secs(5),
        futures::future::join_all((0..8).map(|_| metastore.assign_files_for_indexing(assignment))),
    )
    .await
    .unwrap();
    let mut assigned_ids: HashSet<String> = first
        .staged_files
        .into_iter()
        .map(|file| file.file_id)
        .collect();
    for response in replies {
        for file in response.unwrap().staged_files {
            assert_eq!(file.index_uid(), &index_uid);
            assert!(file.processing_started_timestamp.is_some());
            assert!(assigned_ids.insert(file.file_id));
        }
    }
    assert_eq!(assigned_ids.len(), requests.len() - 1);
    assert!(!assigned_ids.contains(&requests[0].file_id));
    tx.commit().await.unwrap();
    let last = metastore
        .assign_files_for_indexing(assignment)
        .await
        .unwrap();
    assert_eq!(last.staged_files.len(), 1);
    assert_eq!(last.staged_files[0].file_id, requests[0].file_id);
    delete_index(&mut metastore, index_uid).await;
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_assignment_and_publication_lock_order_with_index_deletion() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let requests = register_files(&metastore, &index_uid, 1).await;
    let mut tx = metastore.connection_pool.begin().await.unwrap();
    let deleting_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(tx.as_mut())
        .await
        .unwrap();
    sqlx::query("SELECT index_uid FROM indexes WHERE index_uid = $1 FOR UPDATE")
        .bind(&index_uid)
        .execute(tx.as_mut())
        .await
        .unwrap();

    // Assignment must finish while deletion holds the parent lock. Updating a processing
    // timestamp must not acquire a foreign-key lock on the unchanged index_uid.
    let assigned = timeout(
        Duration::from_secs(5),
        metastore.assign_files_for_indexing(AssignFilesForIndexing {
            max_num_files: 1,
            max_num_bytes: 1_000,
            retry_after_secs: 60,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(assigned.staged_files[0].file_id, requests[0].file_id);
    let publisher = metastore.clone();
    let publication = PublishFileSplits {
        index_uid: Some(index_uid.clone()),
        file_ids: vec![requests[0].file_id.clone()],
        staged_split_ids: Vec::new(),
        publication_id: Ulid::new().to_string(),
    };
    let publishing = tokio::spawn(async move { publisher.publish_file_splits(publication).await });
    wait_for_blocked_query(&metastore, deleting_pid).await;
    // If publication took input locks before its parent lock, this DELETE would deadlock.
    timeout(
        Duration::from_secs(5),
        sqlx::query("DELETE FROM indexes WHERE index_uid = $1")
            .bind(&index_uid)
            .execute(tx.as_mut()),
    )
    .await
    .unwrap()
    .unwrap();
    tx.commit().await.unwrap();
    let result = timeout(Duration::from_secs(5), publishing)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(MetastoreError::NotFound(EntityKind::Index { .. }))
    ));
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_publication_winner_survives_reopen() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let request = registration_request(index_uid.clone());
    read_file(&metastore, &request).await;
    let output = stage_output(&metastore, &index_uid).await;
    let winner = PublishFileSplits {
        index_uid: Some(index_uid.clone()),
        file_ids: vec![request.file_id.clone()],
        staged_split_ids: vec![output.clone()],
        publication_id: Ulid::new().to_string(),
    };
    assert!(
        metastore
            .publish_file_splits(winner.clone())
            .await
            .unwrap()
            .published
    );
    let completed = read_file(&metastore, &request).await;
    assert!(completed.completed_timestamp.is_some());
    assert_eq!(
        completed.publication_id.as_ref(),
        Some(&winner.publication_id)
    );
    drop(metastore);

    // An independent pool must recover the winner and the exact retry answer from storage.
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    assert_eq!(read_file(&metastore, &request).await, completed);
    assert!(
        metastore
            .publish_file_splits(winner.clone())
            .await
            .unwrap()
            .published
    );
    let loser = PublishFileSplits {
        publication_id: Ulid::new().to_string(),
        ..winner
    };
    assert!(
        !metastore
            .publish_file_splits(loser)
            .await
            .unwrap()
            .published
    );
    assert_eq!(read_file(&metastore, &request).await, completed);
    assert_eq!(
        split_state(&metastore, &index_uid, &output).await,
        "Published"
    );
    delete_index(&mut metastore, index_uid).await;
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_split_mutations_wait_in_publication_order() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    // Force heap scans for cleanup. Reversed request vectors alone do not determine row order.
    let options = metastore
        .uri
        .as_str()
        .parse::<sqlx::postgres::PgConnectOptions>()
        .unwrap()
        .options([
            ("enable_indexscan", "off"),
            ("enable_indexonlyscan", "off"),
            ("enable_bitmapscan", "off"),
        ]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let writer = PostgresqlMetastore {
        uri: metastore.uri.clone(),
        connection_pool: super::TrackedPool::new(pool, super::MetastoreKind::Primary),
    };
    for operation in ["stage", "mark", "delete"] {
        let index_uid = create_index(&mut metastore).await;
        let request = registration_request(index_uid.clone());
        read_file(&metastore, &request).await;
        // Insert on one connection so its cached heap page puts z before a, opposing
        // publication's split-ID order. Verify that the fixture exercises this order.
        stage_splits(&writer, &index_uid, &["z", "a"]).await;
        let heap_order: Vec<String> =
            sqlx::query_scalar("SELECT split_id FROM splits WHERE index_uid = $1 ORDER BY ctid")
                .bind(&index_uid)
                .fetch_all(&metastore.connection_pool)
                .await
                .unwrap();
        assert_eq!(heap_order, ["z", "a"]);
        let mut held = metastore.connection_pool.begin().await.unwrap();
        let holding_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(held.as_mut())
            .await
            .unwrap();
        sqlx::query(
            "SELECT split_id FROM splits WHERE index_uid = $1 AND split_id = 'a' FOR UPDATE",
        )
        .bind(&index_uid)
        .execute(held.as_mut())
        .await
        .unwrap();
        let writer = writer.clone();
        let writing_index = index_uid.clone();
        let writing = tokio::spawn(async move {
            let split_ids = vec!["z".to_string(), "a".to_string()];
            match operation {
                "stage" => {
                    let metadata = split_ids.iter().map(|split_id| SplitMetadata {
                        index_uid: writing_index.clone(),
                        split_id: split_id.clone().into(),
                        ..Default::default()
                    });
                    writer
                        .stage_splits(
                            StageSplitsRequest::try_from_splits_metadata(
                                writing_index.clone(),
                                metadata,
                            )
                            .unwrap(),
                        )
                        .await
                }
                "mark" => {
                    writer
                        .mark_splits_for_deletion(MarkSplitsForDeletionRequest {
                            index_uid: Some(writing_index),
                            split_ids,
                        })
                        .await
                }
                "delete" => {
                    writer
                        .delete_splits(DeleteSplitsRequest {
                            index_uid: Some(writing_index),
                            split_ids,
                        })
                        .await
                }
                _ => unreachable!(),
            }
        });
        wait_for_blocked_query(&metastore, holding_pid).await;
        // The writer must wait on a before locking z. A publisher of z can therefore finish
        // while a stays locked. Without sorting, the writer locks z first and blocks publication.
        let published = timeout(
            Duration::from_secs(5),
            metastore.publish_file_splits(PublishFileSplits {
                index_uid: Some(index_uid.clone()),
                file_ids: vec![request.file_id],
                staged_split_ids: vec!["z".to_string()],
                publication_id: Ulid::new().to_string(),
            }),
        )
        .await;
        held.rollback().await.unwrap();
        let result = timeout(Duration::from_secs(5), writing)
            .await
            .unwrap()
            .unwrap();
        delete_index(&mut metastore, index_uid).await;
        assert!(
            published
                .unwrap_or_else(|_| panic!("{operation} locked z before a"))
                .unwrap()
                .published
        );
        if operation == "mark" {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(MetastoreError::FailedPrecondition { .. })),
                "{operation}: {result:?}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::file_serial]
async fn test_publication_rolls_back_outputs_when_input_completion_fails() {
    let mut metastore = PostgresqlMetastore::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let requests = register_files(&metastore, &index_uid, 2).await;
    let output = stage_output(&metastore, &index_uid).await;
    let publication = PublishFileSplits {
        index_uid: Some(index_uid.clone()),
        file_ids: requests
            .iter()
            .map(|request| request.file_id.clone())
            .collect(),
        staged_split_ids: vec![output.clone()],
        publication_id: Ulid::new().to_string(),
    };
    // Inject a database failure after the split UPDATE. The constraint is scoped to this file
    // and removed before assertions so even an unexpectedly successful RPC leaves no fault.
    let constraint = format!("reject_completion_{}", Ulid::new());
    let sql = format!(
        "ALTER TABLE staged_files ADD CONSTRAINT {constraint} CHECK (file_id <> '{}' OR \
         completed_timestamp IS NULL)",
        requests[1].file_id,
    );
    sqlx::query(&sql)
        .execute(&metastore.connection_pool)
        .await
        .unwrap();
    let failed = metastore.publish_file_splits(publication.clone()).await;
    sqlx::query(&format!(
        "ALTER TABLE staged_files DROP CONSTRAINT {constraint}"
    ))
    .execute(&metastore.connection_pool)
    .await
    .unwrap();
    assert!(failed.is_err());
    assert_eq!(split_state(&metastore, &index_uid, &output).await, "Staged");
    for request in &requests {
        let file = read_file(&metastore, request).await;
        assert!(file.completed_timestamp.is_none());
        assert!(file.publication_id.is_none());
    }
    assert!(
        metastore
            .publish_file_splits(publication)
            .await
            .unwrap()
            .published
    );
    assert_eq!(
        split_state(&metastore, &index_uid, &output).await,
        "Published"
    );
    delete_index(&mut metastore, index_uid).await;
}
