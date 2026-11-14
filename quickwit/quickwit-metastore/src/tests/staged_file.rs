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

use async_trait::async_trait;
use quickwit_common::rand::append_random_suffix;
use quickwit_config::IndexConfig;
use quickwit_proto::metastore::{
    AssignFilesForIndexing, CreateIndexRequest, DeleteIndexRequest, DocumentsStagedForUpload,
    EntityKind, MetastoreError, MetastoreService, PublishFileSplits, RegisterFileForIndexing,
};
use quickwit_proto::types::IndexUid;
use ulid::Ulid;

use super::DefaultForTest;
use crate::CreateIndexRequestExt;

/// Adjusts timestamps for deterministic ordering and expiry tests.
/// Lifecycle transitions use the assignment and publication APIs.
#[async_trait]
pub trait WriteStagedFileForTest {
    async fn set_staged_file(&self, staged_file: DocumentsStagedForUpload);
}

pub(crate) fn registration_request(index_uid: IndexUid) -> RegisterFileForIndexing {
    let file_id = Ulid::new().to_string();
    RegisterFileForIndexing {
        file_uri: format!("s3://test-bucket/ingesthree/{index_uid}/{file_id}.finished"),
        index_uid: Some(index_uid),
        file_id,
        num_bytes: 100,
    }
}

pub(crate) async fn create_index(metastore: &mut impl MetastoreService) -> IndexUid {
    let index_id = append_random_suffix("test-staged-file");
    let config = IndexConfig::for_test(&index_id, &format!("ram:///{index_id}"));
    metastore
        .create_index(CreateIndexRequest::try_from_index_config(&config).unwrap())
        .await
        .unwrap()
        .index_uid()
        .clone()
}

pub(crate) async fn delete_index(metastore: &mut impl MetastoreService, index_uid: IndexUid) {
    metastore
        .delete_index(DeleteIndexRequest {
            index_uid: Some(index_uid),
        })
        .await
        .unwrap();
}

pub(crate) type GrpcMetastore = quickwit_proto::metastore::MetastoreServiceGrpcClientAdapter<
    quickwit_proto::metastore::metastore_service_grpc_client::MetastoreServiceGrpcClient<
        quickwit_proto::tonic::transport::Channel,
    >,
>;

pub(crate) fn assignment_request() -> AssignFilesForIndexing {
    AssignFilesForIndexing {
        max_num_files: 1000,
        max_num_bytes: i64::MAX as u64,
        retry_after_secs: 3600,
        max_wait_secs: 0,
    }
}

pub(crate) fn publication_request(
    index_uid: &IndexUid,
    files: &[DocumentsStagedForUpload],
    split_ids: &[&str],
) -> PublishFileSplits {
    PublishFileSplits {
        index_uid: Some(index_uid.clone()),
        file_ids: files.iter().map(|file| file.file_id.clone()).collect(),
        staged_split_ids: split_ids
            .iter()
            .map(|split_id| split_id.to_string())
            .collect(),
        publication_id: Ulid::new().to_string(),
    }
}

pub(crate) async fn register_files(
    metastore: &impl MetastoreService,
    index_uid: &IndexUid,
    sizes: &[u64],
) -> Vec<DocumentsStagedForUpload> {
    let mut files = Vec::new();
    for &num_bytes in sizes {
        let request = RegisterFileForIndexing {
            num_bytes,
            ..registration_request(index_uid.clone())
        };
        files.push(
            metastore
                .register_file_for_indexing(request)
                .await
                .unwrap()
                .staged_file
                .unwrap(),
        );
    }
    files
}

/// Registration retries expose the authoritative record without changing its progress.
pub(crate) async fn read_file(
    metastore: &impl MetastoreService,
    file: &DocumentsStagedForUpload,
) -> DocumentsStagedForUpload {
    metastore
        .register_file_for_indexing(RegisterFileForIndexing {
            index_uid: file.index_uid.clone(),
            file_id: file.file_id.clone(),
            file_uri: file.file_uri.clone(),
            num_bytes: file.num_bytes,
        })
        .await
        .unwrap()
        .staged_file
        .unwrap()
}

pub async fn test_register_file_for_indexing<M: MetastoreService + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let request = registration_request(index_uid.clone());
    let input = metastore
        .register_file_for_indexing(request.clone())
        .await
        .unwrap()
        .staged_file
        .unwrap();
    assert_eq!(input.index_uid, request.index_uid);
    assert_eq!(input.file_id, request.file_id);
    assert_eq!(input.file_uri, request.file_uri);
    assert_eq!(input.num_bytes, request.num_bytes);
    assert!(input.create_timestamp > 0);
    assert_eq!(input.processing_started_timestamp, None);
    assert_eq!(input.completed_timestamp, None);
    assert_eq!(input.publication_id, None);
    // An ignored response is reconciled by retrying the exact request.
    let retry = metastore
        .register_file_for_indexing(request.clone())
        .await
        .unwrap()
        .staged_file
        .unwrap();
    assert_eq!(retry, input);
    for repeated in [
        RegisterFileForIndexing {
            file_uri: "s3://test-bucket/different".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            num_bytes: 101,
            ..request.clone()
        },
    ] {
        assert_eq!(
            metastore
                .register_file_for_indexing(repeated)
                .await
                .unwrap()
                .staged_file
                .unwrap(),
            input
        );
    }
    assert_eq!(
        metastore
            .register_file_for_indexing(request)
            .await
            .unwrap()
            .staged_file
            .unwrap(),
        input
    );
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_register_file_for_indexing_invalid_request<
    M: MetastoreService + DefaultForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let request = registration_request(index_uid.clone());
    let invalid_requests = [
        RegisterFileForIndexing {
            index_uid: None,
            ..request.clone()
        },
        RegisterFileForIndexing {
            index_uid: Some(IndexUid::new_with_random_ulid("")),
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_id: "invalid".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_id: "01arz3ndektsv4rrffq69g5fav".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            num_bytes: 0,
            ..request.clone()
        },
        RegisterFileForIndexing {
            num_bytes: i64::MAX as u64 + 1,
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_uri: "".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_uri: "relative/path".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_uri: "unknown://bucket/file".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_uri: "grpc://localhost:1234".to_string(),
            ..request.clone()
        },
        RegisterFileForIndexing {
            file_uri: "s3://bucket".to_string(),
            ..request.clone()
        },
    ];
    for invalid in invalid_requests {
        assert!(matches!(
            metastore
                .register_file_for_indexing(invalid)
                .await
                .unwrap_err(),
            MetastoreError::InvalidArgument { .. }
        ));
    }
    // Reusing this identity proves invalid requests did not create a partial record.
    let input = metastore
        .register_file_for_indexing(RegisterFileForIndexing {
            num_bytes: i64::MAX as u64,
            ..request
        })
        .await
        .unwrap()
        .staged_file
        .unwrap();
    assert_eq!(input.num_bytes, i64::MAX as u64);
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_register_file_for_indexing_index_incarnation<
    M: MetastoreService + DefaultForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let request = registration_request(index_uid.clone());
    for absent_uid in [
        IndexUid::new_with_random_ulid("missing-staged-file-index"),
        IndexUid::new_with_random_ulid(&index_uid.index_id),
    ] {
        assert!(matches!(
            metastore
                .register_file_for_indexing(RegisterFileForIndexing {
                    index_uid: Some(absent_uid),
                    ..request.clone()
                })
                .await
                .unwrap_err(),
            MetastoreError::NotFound(EntityKind::Index { .. })
        ));
    }
    metastore
        .register_file_for_indexing(request.clone())
        .await
        .unwrap();
    delete_index(&mut metastore, index_uid.clone()).await;
    assert!(matches!(
        metastore
            .register_file_for_indexing(request.clone())
            .await
            .unwrap_err(),
        MetastoreError::NotFound(EntityKind::Index { .. })
    ));
    let config = IndexConfig::for_test(&index_uid.index_id, "ram:///recreated-index");
    let new_uid = metastore
        .create_index(CreateIndexRequest::try_from_index_config(&config).unwrap())
        .await
        .unwrap()
        .index_uid()
        .clone();
    assert_ne!(new_uid, index_uid);
    assert!(matches!(
        metastore
            .register_file_for_indexing(request)
            .await
            .unwrap_err(),
        MetastoreError::NotFound(EntityKind::Index { .. })
    ));
    metastore
        .register_file_for_indexing(registration_request(new_uid.clone()))
        .await
        .unwrap();
    delete_index(&mut metastore, new_uid).await;
}

pub async fn test_register_file_for_indexing_concurrent_retries<
    M: MetastoreService + DefaultForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let request = registration_request(index_uid.clone());
    let requests = (0..16).map(|_| metastore.register_file_for_indexing(request.clone()));
    let replies = futures::future::join_all(requests).await;
    let mut inputs = replies
        .into_iter()
        .map(|reply| reply.unwrap().staged_file.unwrap());
    let first = inputs.next().unwrap();
    for input in inputs {
        assert_eq!(input, first);
    }
    let left = registration_request(index_uid.clone());
    let right = RegisterFileForIndexing {
        num_bytes: left.num_bytes + 1,
        ..left.clone()
    };
    let (left_result, right_result) = tokio::join!(
        metastore.register_file_for_indexing(left.clone()),
        metastore.register_file_for_indexing(right.clone())
    );
    let registered = left_result.unwrap();
    assert_eq!(right_result.unwrap(), registered);
    let num_bytes = registered.staged_file.as_ref().unwrap().num_bytes;
    assert!(num_bytes == left.num_bytes || num_bytes == right.num_bytes);
    assert_eq!(
        metastore.register_file_for_indexing(left).await.unwrap(),
        registered
    );
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_register_file_for_indexing_preserves_progress<
    M: MetastoreService + DefaultForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let request = registration_request(index_uid.clone());
    metastore
        .register_file_for_indexing(request.clone())
        .await
        .unwrap();
    let mut assigned = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    assert_eq!(assigned.len(), 1);
    let mut input = assigned.pop().unwrap();
    assert_eq!(input.file_id, request.file_id);
    assert!(input.processing_started_timestamp.is_some());
    let publication = publication_request(&index_uid, std::slice::from_ref(&input), &[]);
    for completed in [false, true] {
        if completed {
            assert!(
                metastore
                    .publish_file_splits(publication.clone())
                    .await
                    .unwrap()
                    .published
            );
            input = read_file(&metastore, &input).await;
            assert!(input.completed_timestamp.is_some());
            assert_eq!(
                input.publication_id.as_ref(),
                Some(&publication.publication_id)
            );
        }
        let retry = metastore
            .register_file_for_indexing(request.clone())
            .await
            .unwrap()
            .staged_file
            .unwrap();
        assert_eq!(retry, input);
        let repeated = RegisterFileForIndexing {
            num_bytes: 101,
            ..request.clone()
        };
        assert_eq!(
            metastore
                .register_file_for_indexing(repeated)
                .await
                .unwrap()
                .staged_file
                .unwrap(),
            input
        );
        assert_eq!(
            metastore
                .register_file_for_indexing(request.clone())
                .await
                .unwrap()
                .staged_file
                .unwrap(),
            input
        );
    }
    delete_index(&mut metastore, index_uid).await;
}

#[tokio::test]
async fn test_register_file_for_indexing_grpc() {
    test_register_file_for_indexing::<GrpcMetastore>().await;
    test_register_file_for_indexing_invalid_request::<GrpcMetastore>().await;
    test_register_file_for_indexing_index_incarnation::<GrpcMetastore>().await;
    test_register_file_for_indexing_concurrent_retries::<GrpcMetastore>().await;
    test_register_file_for_indexing_preserves_progress::<GrpcMetastore>().await;
}
