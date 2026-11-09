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

use itertools::Itertools;
use quickwit_proto::metastore::{
    AssignFilesForIndexing, DocumentsStagedForUpload, EntityKind, MetastoreError, MetastoreResult,
    PublishFileSplits, RegisterFileForIndexing,
};
use time::OffsetDateTime;

use super::FileBackedIndex;
use crate::file_backed::MutationOccurred;
use crate::metastore::staged_file::{assignment_file_count, is_eligible, publication_outcome};

impl FileBackedIndex {
    pub fn register_file_for_indexing(
        &mut self,
        request: RegisterFileForIndexing,
    ) -> MetastoreResult<MutationOccurred<DocumentsStagedForUpload>> {
        if let Some(existing) = self.staged_files.get(&request.file_id) {
            return Ok(MutationOccurred::No(existing.clone()));
        }
        let staged_file = DocumentsStagedForUpload {
            index_uid: request.index_uid,
            file_id: request.file_id,
            file_uri: request.file_uri,
            num_bytes: request.num_bytes,
            create_timestamp: OffsetDateTime::now_utc().unix_timestamp(),
            processing_started_timestamp: None,
            completed_timestamp: None,
            publication_id: None,
        };
        self.staged_files
            .insert(staged_file.file_id.clone(), staged_file.clone());
        Ok(MutationOccurred::Yes(staged_file))
    }

    pub(crate) fn list_completed_files(
        &self,
        after_file_id: &str,
        max_num_files: usize,
    ) -> Vec<DocumentsStagedForUpload> {
        self.staged_files
            .values()
            .filter(|file| {
                file.completed_timestamp.is_some() && file.file_id.as_str() > after_file_id
            })
            .k_smallest_by(max_num_files, |left, right| {
                left.file_id.cmp(&right.file_id)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn oldest_eligible_file(&self, retry_before: i64) -> Option<(i64, String)> {
        self.staged_files
            .values()
            .filter(|file| is_eligible(file, retry_before))
            .min_by_key(|file| (file.create_timestamp, &file.file_id))
            .map(|file| (file.create_timestamp, file.file_id.clone()))
    }

    /// The caller holds the exclusive metastore mutation guard from global selection through
    /// persistence, and supplies the same timestamp/cutoff used to select this index.
    pub(crate) fn assign_files_for_indexing(
        &mut self,
        request: &AssignFilesForIndexing,
        now: i64,
        retry_before: i64,
    ) -> MutationOccurred<Vec<DocumentsStagedForUpload>> {
        let mut files: Vec<_> = self
            .staged_files
            .values()
            .filter(|file| is_eligible(file, retry_before))
            .collect();
        if files.is_empty() {
            return MutationOccurred::No(Vec::new());
        }
        files.sort_unstable_by(|left, right| {
            (left.create_timestamp, &left.file_id).cmp(&(right.create_timestamp, &right.file_id))
        });
        files.truncate(request.max_num_files as usize);
        files.truncate(assignment_file_count(
            files.iter().map(|file| file.num_bytes),
            request.max_num_bytes,
        ));
        let mut files: Vec<_> = files.into_iter().cloned().collect();
        for file in &mut files {
            file.processing_started_timestamp = Some(now);
            self.staged_files.insert(file.file_id.clone(), file.clone());
        }
        MutationOccurred::Yes(files)
    }

    /// Called only on the private clone owned by `FileBackedMetastore::mutate`, so split
    /// validation errors discard every intermediate change before anything reaches storage.
    pub(crate) fn publish_file_splits(
        &mut self,
        request: PublishFileSplits,
    ) -> MetastoreResult<MutationOccurred<bool>> {
        let mut files = Vec::with_capacity(request.file_ids.len());
        for file_id in &request.file_ids {
            let file = self.staged_files.get(file_id).ok_or_else(|| {
                MetastoreError::FailedPrecondition {
                    entity: EntityKind::Index {
                        index_id: self.index_id().to_string(),
                    },
                    message: format!("staged file `{file_id}` does not exist"),
                }
            })?;
            files.push(file.clone());
        }
        // Retained completion rows recognize retries even after output splits are deleted.
        if let Some(published) = publication_outcome(&files, &request.publication_id) {
            return Ok(MutationOccurred::No(published));
        }
        self.mark_splits_as_published_helper(request.staged_split_ids)?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        for mut file in files {
            file.completed_timestamp = Some(now);
            file.publication_id = Some(request.publication_id.clone());
            self.staged_files.insert(file.file_id.clone(), file);
        }
        Ok(MutationOccurred::Yes(true))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures::executor::block_on;
    use quickwit_proto::metastore::{DeleteIndexRequest, MetastoreService, StageSplitsRequest};
    use quickwit_storage::{MockStorage, RamStorage, Storage, StorageErrorKind};
    use ulid::Ulid;

    use super::*;
    use crate::file_backed::store_operations::metastore_filepath;
    use crate::tests::staged_file::{WriteStagedFileForTest, create_index, registration_request};
    use crate::{FileBackedMetastore, SplitMetadata, SplitState, StageSplitsRequestExt};

    #[async_trait]
    impl WriteStagedFileForTest for FileBackedMetastore {
        async fn set_staged_file(&self, staged_file: DocumentsStagedForUpload) {
            self.mutate(&staged_file.index_uid().clone(), |index| {
                assert!(index.staged_files.contains_key(&staged_file.file_id));
                index
                    .staged_files
                    .insert(staged_file.file_id.clone(), staged_file);
                Ok(MutationOccurred::Yes(()))
            })
            .await
            .unwrap();
        }
    }

    #[test]
    fn test_assign_files_without_eligible_files_does_not_mutate() {
        let mut index = FileBackedIndex::from(crate::IndexMetadata::for_test(
            "empty-assignment",
            "ram:///indexes/empty-assignment",
        ));
        let request = AssignFilesForIndexing {
            max_num_files: 10,
            max_num_bytes: 1000,
            retry_after_secs: 60,
        };
        assert!(matches!(
            index.assign_files_for_indexing(&request, 100, 40),
            MutationOccurred::No(files) if files.is_empty()
        ));

        let registration = registration_request(index.index_uid().clone());
        index
            .register_file_for_indexing(registration.clone())
            .unwrap();
        assert!(matches!(
            index.assign_files_for_indexing(&request, 100, 40),
            MutationOccurred::Yes(files) if files.len() == 1
        ));
        let assigned = index.staged_files.clone();
        assert!(matches!(
            index.assign_files_for_indexing(&request, 100, 40),
            MutationOccurred::No(files) if files.is_empty()
        ));
        assert_eq!(index.staged_files, assigned);

        index
            .publish_file_splits(PublishFileSplits {
                index_uid: registration.index_uid,
                file_ids: vec![registration.file_id],
                staged_split_ids: Vec::new(),
                publication_id: Ulid::new().to_string(),
            })
            .unwrap();
        let completed = index.staged_files.clone();
        assert!(matches!(
            index.assign_files_for_indexing(&request, 200, 140),
            MutationOccurred::No(files) if files.is_empty()
        ));
        assert_eq!(index.staged_files, completed);
    }

    #[tokio::test]
    async fn test_assign_files_propagates_write_not_found() {
        let storage = Arc::new(RamStorage::default());
        let mut metastore = FileBackedMetastore::try_new(storage, None).await.unwrap();
        let index_uid = create_index(&mut metastore).await;
        metastore
            .register_file_for_indexing(registration_request(index_uid))
            .await
            .unwrap();
        let mut mock_storage = MockStorage::default();
        mock_storage.expect_put().times(1).returning(|_, _| {
            Err(StorageErrorKind::NotFound.with_error(anyhow::anyhow!("write failed")))
        });
        metastore.storage = Arc::new(mock_storage);
        assert!(matches!(
            metastore
                .assign_files_for_indexing(AssignFilesForIndexing {
                    max_num_files: 1,
                    max_num_bytes: 100,
                    retry_after_secs: 60,
                })
                .await
                .unwrap_err(),
            MetastoreError::NotFound(EntityKind::Index { .. })
        ));
    }

    #[tokio::test]
    async fn test_file_mutations_exclude_index_deletion() {
        for publish in [false, true] {
            let storage = Arc::new(RamStorage::default());
            let mut metastore = FileBackedMetastore::try_new(storage.clone(), None)
                .await
                .unwrap();
            let index_uid = create_index(&mut metastore).await;
            let input = metastore
                .register_file_for_indexing(registration_request(index_uid.clone()))
                .await
                .unwrap()
                .staged_file
                .unwrap();
            let locked_index = metastore
                .get_locked_index(&index_uid.index_id)
                .await
                .unwrap();
            let processing = async {
                if publish {
                    metastore
                        .publish_file_splits(PublishFileSplits {
                            index_uid: Some(index_uid.clone()),
                            file_ids: vec![input.file_id],
                            staged_split_ids: Vec::new(),
                            publication_id: Ulid::new().to_string(),
                        })
                        .await
                        .unwrap();
                } else {
                    metastore
                        .assign_files_for_indexing(AssignFilesForIndexing {
                            max_num_files: 1,
                            max_num_bytes: 100,
                            retry_after_secs: 60,
                        })
                        .await
                        .unwrap();
                }
            };
            tokio::pin!(processing);
            assert!(futures::poll!(&mut processing).is_pending());
            let deletion = metastore.delete_index(DeleteIndexRequest {
                index_uid: Some(index_uid.clone()),
            });
            tokio::pin!(deletion);
            assert!(futures::poll!(&mut deletion).is_pending());
            drop(locked_index);
            processing.await;
            deletion.await.unwrap();
            assert!(
                !storage
                    .exists(&metastore_filepath(&index_uid.index_id))
                    .await
                    .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn test_publish_file_splits_ambiguous_write_and_reopen() {
        let storage = Arc::new(RamStorage::default());
        let mut metastore = FileBackedMetastore::try_new(storage.clone(), None)
            .await
            .unwrap();
        let index_uid = create_index(&mut metastore).await;
        let input = metastore
            .register_file_for_indexing(registration_request(index_uid.clone()))
            .await
            .unwrap()
            .staged_file
            .unwrap();
        let assignment = AssignFilesForIndexing {
            max_num_files: 10,
            max_num_bytes: 1000,
            retry_after_secs: 60,
        };
        let assigned = metastore
            .assign_files_for_indexing(assignment)
            .await
            .unwrap();
        assert_eq!(assigned.staged_files.len(), 1);
        let split = SplitMetadata::for_test("file-output".into());
        metastore
            .stage_splits(
                StageSplitsRequest::try_from_split_metadata(index_uid.clone(), &split).unwrap(),
            )
            .await
            .unwrap();
        drop(metastore);

        let mut mock_storage = MockStorage::default();
        let exists_storage = storage.clone();
        mock_storage
            .expect_exists()
            .returning(move |path| block_on(exists_storage.exists(path)));
        let read_storage = storage.clone();
        mock_storage
            .expect_get_all()
            .returning(move |path| block_on(read_storage.get_all(path)));
        let write_storage = storage.clone();
        mock_storage
            .expect_put()
            .times(1)
            .returning(move |path, payload| {
                block_on(write_storage.put(path, payload))?;
                Err(StorageErrorKind::Io.with_error(anyhow::anyhow!("write response lost")))
            });
        let metastore = FileBackedMetastore::try_new(Arc::new(mock_storage), None)
            .await
            .unwrap();
        let before = metastore.get_index(&index_uid).await.unwrap();
        assert_eq!(
            before.staged_files[&input.file_id],
            assigned.staged_files[0]
        );
        let publication = PublishFileSplits {
            index_uid: Some(index_uid.clone()),
            file_ids: vec![input.file_id.clone()],
            staged_split_ids: vec![split.split_id.to_string()],
            publication_id: Ulid::new().to_string(),
        };
        assert!(matches!(
            metastore
                .publish_file_splits(publication.clone())
                .await
                .unwrap_err(),
            MetastoreError::Internal { .. }
        ));
        // The failed response invalidated the cache. The retry reloads completion evidence
        // and succeeds without another write (the mock permits exactly one put).
        assert!(
            metastore
                .publish_file_splits(publication.clone())
                .await
                .unwrap()
                .published
        );
        drop(metastore);

        let reopened = FileBackedMetastore::try_new(storage, None).await.unwrap();
        let index = reopened.get_index(&index_uid).await.unwrap();
        assert_eq!(
            index.splits[&split.split_id].split_state,
            SplitState::Published
        );
        assert!(
            index.staged_files[&input.file_id]
                .completed_timestamp
                .is_some()
        );
        assert_eq!(
            index.staged_files[&input.file_id].publication_id.as_deref(),
            Some(publication.publication_id.as_str())
        );
        assert!(
            reopened
                .assign_files_for_indexing(assignment)
                .await
                .unwrap()
                .staged_files
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_register_file_for_indexing_reopen() {
        let storage = Arc::new(RamStorage::default());
        let mut metastore = FileBackedMetastore::try_new(storage.clone(), None)
            .await
            .unwrap();
        let index_uid = create_index(&mut metastore).await;
        let request = registration_request(index_uid.clone());
        let mut input = metastore
            .register_file_for_indexing(request.clone())
            .await
            .unwrap()
            .staged_file
            .unwrap();
        input.processing_started_timestamp = Some(input.create_timestamp + 1);
        input.completed_timestamp = Some(input.create_timestamp + 2);
        metastore.set_staged_file(input.clone()).await;
        drop(metastore);
        let metastore_path = metastore_filepath(&index_uid.index_id);
        let before = storage.get_all(&metastore_path).await.unwrap();
        let reopened = FileBackedMetastore::try_new(storage.clone(), None)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .register_file_for_indexing(request)
                .await
                .unwrap()
                .staged_file
                .unwrap(),
            input
        );
        let after = storage.get_all(&metastore_path).await.unwrap();
        assert_eq!(before.as_slice(), after.as_slice());
        reopened
            .delete_index(DeleteIndexRequest {
                index_uid: Some(index_uid),
            })
            .await
            .unwrap();
    }
}
