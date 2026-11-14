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

use quickwit_proto::metastore::{
    AssignFilesForIndexing, AssignFilesForIndexingResponse, DocumentsStagedForUpload, EntityKind,
    ListCompletedFilesRequest, ListCompletedFilesResponse, MetastoreError, MetastoreResult,
    PublishFileSplits, PublishFileSplitsResponse, RegisterFileForIndexing,
    RegisterFileForIndexingResponse,
};
use quickwit_proto::types::IndexUid;
use sqlx::{Acquire, Postgres};
use time::OffsetDateTime;

use super::pool::TrackedPool;
use crate::SplitState;
use crate::metastore::staged_file::{
    assignment_file_count, assignment_is_ready, publication_outcome, validate_assignment,
    validate_completed_files_request, validate_publication, validate_registration,
};

#[derive(sqlx::FromRow)]
struct PgStagedFile {
    #[sqlx(try_from = "String")]
    index_uid: IndexUid,
    file_id: String,
    file_uri: String,
    num_bytes: i64,
    create_timestamp: OffsetDateTime,
    processing_started_timestamp: Option<OffsetDateTime>,
    completed_timestamp: Option<OffsetDateTime>,
    publication_id: Option<String>,
}

impl From<PgStagedFile> for DocumentsStagedForUpload {
    fn from(input: PgStagedFile) -> Self {
        Self {
            index_uid: Some(input.index_uid),
            file_id: input.file_id,
            file_uri: input.file_uri,
            num_bytes: input.num_bytes as u64,
            create_timestamp: input.create_timestamp.unix_timestamp(),
            processing_started_timestamp: input
                .processing_started_timestamp
                .map(|time| time.unix_timestamp()),
            completed_timestamp: input.completed_timestamp.map(|time| time.unix_timestamp()),
            publication_id: input.publication_id,
        }
    }
}

pub(super) async fn register_file_for_indexing(
    pool: &TrackedPool<Postgres>,
    request: RegisterFileForIndexing,
) -> MetastoreResult<RegisterFileForIndexingResponse> {
    validate_registration(&request)?;
    let index_uid = request.index_uid();
    let mut tx = pool.begin().await?;

    // Keep index deletion from removing the staged file between INSERT and SELECT. A shared
    // key lock permits concurrent registrations and pins this exact index incarnation.
    let index_exists: Option<String> =
        sqlx::query_scalar("SELECT index_uid FROM indexes WHERE index_uid = $1 FOR KEY SHARE")
            .bind(index_uid)
            .fetch_optional(tx.as_mut())
            .await?;
    if index_exists.is_none() {
        return Err(MetastoreError::NotFound(EntityKind::Index {
            index_id: index_uid.index_id.clone(),
        }));
    }
    sqlx::query(
        "INSERT INTO staged_files (index_uid, file_id, file_uri, num_bytes)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (file_id) DO NOTHING",
    )
    .bind(index_uid)
    .bind(&request.file_id)
    .bind(&request.file_uri)
    .bind(request.num_bytes as i64)
    .execute(tx.as_mut())
    .await?;

    // Use a separate statement: under READ COMMITTED, an INSERT that waited on a
    // conflicting registration can now see its committed row. A single CTE using
    // the INSERT's snapshot could miss it. Retries never UPDATE the existing row.
    let staged_file: DocumentsStagedForUpload =
        sqlx::query_as::<_, PgStagedFile>("SELECT * FROM staged_files WHERE file_id = $1")
            .bind(&request.file_id)
            .fetch_one(tx.as_mut())
            .await?
            .into();
    tx.commit().await?;
    Ok(RegisterFileForIndexingResponse {
        staged_file: Some(staged_file),
    })
}

pub(super) async fn assign_files_for_indexing(
    pool: &TrackedPool<Postgres>,
    request: AssignFilesForIndexing,
) -> MetastoreResult<AssignFilesForIndexingResponse> {
    validate_assignment(&request)?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let retry_before = now - i64::from(request.retry_after_secs);
    let mut waiting_indexes: Vec<String> = Vec::new();

    loop {
        // Rejected groups release their locks before trying another index. Only lock
        // files: a parent lock would invert publication/deletion's index -> file order.
        let mut tx = pool.begin().await?;
        let oldest = sqlx::query_as::<_, PgStagedFile>(
            "SELECT * FROM staged_files
             WHERE completed_timestamp IS NULL AND NOT (index_uid = ANY($2))
               AND (processing_started_timestamp IS NULL OR processing_started_timestamp < \
             to_timestamp($1))
             ORDER BY create_timestamp, file_id
             LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .bind(retry_before as f64)
        .bind(&waiting_indexes)
        .fetch_optional(tx.as_mut())
        .await?;
        let Some(oldest) = oldest else {
            tx.commit().await?;
            return Ok(AssignFilesForIndexingResponse::default());
        };
        let index_uid = oldest.index_uid.clone();
        // Earlier locked rows stay outside this group even if unlocked between queries.
        // Limit the rows read per index instead of aggregating the whole backlog.
        let additional = sqlx::query_as::<_, PgStagedFile>(
            "SELECT * FROM staged_files
             WHERE index_uid = $1 AND completed_timestamp IS NULL
               AND (processing_started_timestamp IS NULL OR processing_started_timestamp < \
             to_timestamp($2))
               AND (create_timestamp, file_id) > ($3, $4)
             ORDER BY create_timestamp, file_id
             LIMIT $5 FOR UPDATE SKIP LOCKED",
        )
        .bind(&index_uid)
        .bind(retry_before as f64)
        .bind(oldest.create_timestamp)
        .bind(&oldest.file_id)
        .bind(i64::from(request.max_num_files - 1))
        .fetch_all(tx.as_mut())
        .await?;
        let mut staged_files: Vec<DocumentsStagedForUpload> = std::iter::once(oldest)
            .chain(additional)
            .map(Into::into)
            .collect();
        staged_files.truncate(assignment_file_count(
            staged_files.iter().map(|file| file.num_bytes),
            request.max_num_bytes,
        ));
        // Only locked files contribute bytes and age. An underfilled index must not
        // prevent assignment from another index during this request.
        if !assignment_is_ready(staged_files.iter(), &request, now) {
            tx.rollback().await?;
            waiting_indexes.push(index_uid.to_string());
            continue;
        }
        let file_ids: Vec<&str> = staged_files
            .iter()
            .map(|file| file.file_id.as_str())
            .collect();
        sqlx::query(
            "UPDATE staged_files SET processing_started_timestamp = to_timestamp($2)
             WHERE file_id = ANY($1)",
        )
        .bind(&file_ids)
        .bind(now as f64)
        .execute(tx.as_mut())
        .await?;
        for file in &mut staged_files {
            file.processing_started_timestamp = Some(now);
        }
        tx.commit().await?;
        return Ok(AssignFilesForIndexingResponse { staged_files });
    }
}

pub(super) async fn list_completed_files(
    pool: &TrackedPool<Postgres>,
    request: ListCompletedFilesRequest,
) -> MetastoreResult<ListCompletedFilesResponse> {
    validate_completed_files_request(&request)?;
    let staged_files = sqlx::query_as::<_, PgStagedFile>(
        "SELECT * FROM staged_files
         WHERE completed_timestamp IS NOT NULL AND file_id > $1
         ORDER BY file_id ASC LIMIT $2",
    )
    .bind(&request.after_file_id)
    .bind(i64::from(request.max_num_files))
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(Into::into)
    .collect();
    Ok(ListCompletedFilesResponse { staged_files })
}

pub(super) async fn publish_file_splits(
    pool: &TrackedPool<Postgres>,
    request: PublishFileSplits,
) -> MetastoreResult<PublishFileSplitsResponse> {
    validate_publication(&request)?;
    let index_uid = request.index_uid();
    let mut tx = pool.begin().await?;

    // Pin the index incarnation before locking inputs, matching cascading index deletion.
    // KEY SHARE allows disjoint publications concurrently and conflicts with the index lock
    // taken by legacy publish_splits, which also updates splits.
    let index_exists: Option<String> =
        sqlx::query_scalar("SELECT index_uid FROM indexes WHERE index_uid = $1 FOR KEY SHARE")
            .bind(index_uid)
            .fetch_optional(tx.as_mut())
            .await?;
    if index_exists.is_none() {
        return Err(MetastoreError::NotFound(EntityKind::Index {
            index_id: index_uid.index_id.clone(),
        }));
    }
    // Overlapping groups must acquire their input locks in the same order, regardless of
    // request order. Assignment uses SKIP LOCKED and never waits on another input lock.
    let files: Vec<DocumentsStagedForUpload> = sqlx::query_as::<_, PgStagedFile>(
        "SELECT * FROM staged_files WHERE index_uid = $1 AND file_id = ANY($2)
         ORDER BY file_id FOR UPDATE",
    )
    .bind(index_uid)
    .bind(&request.file_ids)
    .fetch_all(tx.as_mut())
    .await?
    .into_iter()
    .map(Into::into)
    .collect();
    if files.len() != request.file_ids.len() {
        return Err(MetastoreError::FailedPrecondition {
            entity: EntityKind::Index {
                index_id: index_uid.index_id.clone(),
            },
            message: "input files are missing from the index".to_string(),
        });
    }
    // Completion is the durable winner evidence. Check it before outputs, which may already
    // have been merged or garbage collected. Assignment timestamps never fence a publication.
    if let Some(published) = publication_outcome(&files, &request.publication_id) {
        tx.commit().await?;
        return Ok(PublishFileSplitsResponse { published });
    }
    let splits: Vec<(String, String)> = sqlx::query_as(
        "SELECT split_id, split_state FROM splits
         WHERE index_uid = $1 AND split_id = ANY($2)
         ORDER BY split_id FOR UPDATE",
    )
    .bind(index_uid)
    .bind(&request.staged_split_ids)
    .fetch_all(tx.as_mut())
    .await?;
    if splits.len() != request.staged_split_ids.len() {
        let missing_split_ids = request
            .staged_split_ids
            .iter()
            .filter(|split_id| !splits.iter().any(|(found_id, _)| found_id == *split_id))
            .cloned()
            .collect();
        return Err(MetastoreError::NotFound(EntityKind::Splits {
            split_ids: missing_split_ids,
        }));
    }
    let not_staged_split_ids: Vec<String> = splits
        .into_iter()
        .filter(|(_, state)| state != SplitState::Staged.as_str())
        .map(|(split_id, _)| split_id)
        .collect();
    if !not_staged_split_ids.is_empty() {
        return Err(MetastoreError::FailedPrecondition {
            entity: EntityKind::Splits {
                split_ids: not_staged_split_ids,
            },
            message: "splits are not staged".to_string(),
        });
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query(
        "UPDATE splits SET split_state = 'Published',
             update_timestamp = to_timestamp($3) AT TIME ZONE 'UTC',
             publish_timestamp = to_timestamp($3) AT TIME ZONE 'UTC'
         WHERE index_uid = $1 AND split_id = ANY($2)",
    )
    .bind(index_uid)
    .bind(&request.staged_split_ids)
    .bind(now as f64)
    .execute(tx.as_mut())
    .await?;
    sqlx::query(
        "UPDATE staged_files SET completed_timestamp = to_timestamp($3), publication_id = $4
         WHERE index_uid = $1 AND file_id = ANY($2)",
    )
    .bind(index_uid)
    .bind(&request.file_ids)
    .bind(now as f64)
    .bind(&request.publication_id)
    .execute(tx.as_mut())
    .await?;
    tx.commit().await?;
    Ok(PublishFileSplitsResponse { published: true })
}

#[cfg(test)]
mod tests {
    use quickwit_proto::metastore::{DeleteIndexRequest, MetastoreService};

    use super::*;
    use crate::PostgresqlMetastore;
    use crate::tests::DefaultForTest;
    use crate::tests::staged_file::{WriteStagedFileForTest, create_index, registration_request};

    #[tokio::test]
    #[serial_test::file_serial]
    async fn test_register_file_for_indexing_preserves_registration() {
        let mut metastore = PostgresqlMetastore::default_for_test().await;
        let index_uid = create_index(&mut metastore).await;
        let other_index_uid = create_index(&mut metastore).await;
        let request = registration_request(index_uid.clone());
        let mut existing = metastore
            .register_file_for_indexing(request.clone())
            .await
            .unwrap();
        let other_index_request = RegisterFileForIndexing {
            index_uid: Some(other_index_uid.clone()),
            ..request.clone()
        };
        for (processing_started, completed) in
            [(None, None), (Some(20), None), (Some(20), Some(30))]
        {
            let staged_file = existing.staged_file.as_mut().unwrap();
            staged_file.create_timestamp = 10;
            staged_file.processing_started_timestamp = processing_started;
            staged_file.completed_timestamp = completed;
            metastore.set_staged_file(staged_file.clone()).await;
            assert_eq!(
                metastore
                    .register_file_for_indexing(other_index_request.clone())
                    .await
                    .unwrap(),
                existing
            );
            assert_eq!(
                metastore
                    .register_file_for_indexing(request.clone())
                    .await
                    .unwrap(),
                existing
            );
        }
        for index_uid in [index_uid, other_index_uid] {
            metastore
                .delete_index(DeleteIndexRequest {
                    index_uid: Some(index_uid),
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[serial_test::file_serial]
    async fn test_register_file_for_indexing_concurrent_indexes() {
        let mut metastore = PostgresqlMetastore::default_for_test().await;
        let left_index_uid = create_index(&mut metastore).await;
        let right_index_uid = create_index(&mut metastore).await;
        let left = registration_request(left_index_uid.clone());
        let right = RegisterFileForIndexing {
            index_uid: Some(right_index_uid.clone()),
            ..left.clone()
        };
        let (left_result, right_result) = tokio::join!(
            metastore.register_file_for_indexing(left.clone()),
            metastore.register_file_for_indexing(right.clone())
        );
        let registered = left_result.unwrap();
        assert_eq!(right_result.unwrap(), registered);
        let index_uid = &registered.staged_file.as_ref().unwrap().index_uid;
        assert!(index_uid == &left.index_uid || index_uid == &right.index_uid);
        assert_eq!(
            metastore.register_file_for_indexing(left).await.unwrap(),
            registered
        );
        for index_uid in [left_index_uid, right_index_uid] {
            metastore
                .delete_index(DeleteIndexRequest {
                    index_uid: Some(index_uid),
                })
                .await
                .unwrap();
        }
    }
}
