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
    EntityKind, MarkSplitsForDeletionRequest, MetastoreError, PublishFileSplits,
    PublishSplitsRequest,
};
use quickwit_proto::types::IndexUid;
use ulid::Ulid;

use super::file_publication::{assert_split_states, stage_splits};
use super::staged_file::{
    GrpcMetastore, create_index, publication_request, read_file, register_files,
};
use super::{DefaultForTest, cleanup_index};
use crate::{MetastoreServiceExt, SplitState};

pub async fn test_publish_file_splits_output_rollback<M: MetastoreServiceExt + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100, 100]).await;
    stage_splits(&metastore, &index_uid, &["good", "published", "marked"]).await;
    metastore
        .publish_splits(PublishSplitsRequest {
            index_uid: Some(index_uid.clone()),
            staged_split_ids: vec!["published".to_string()],
            ..Default::default()
        })
        .await
        .unwrap();
    metastore
        .mark_splits_for_deletion(MarkSplitsForDeletionRequest::new(
            index_uid.clone(),
            vec!["marked".to_string()],
        ))
        .await
        .unwrap();

    for bad_split in ["missing", "published", "marked"] {
        let publication = publication_request(&index_uid, &files, &["good", bad_split]);
        let error = metastore
            .publish_file_splits(publication)
            .await
            .unwrap_err();
        if bad_split == "missing" {
            assert!(matches!(
                error,
                MetastoreError::NotFound(EntityKind::Splits { .. })
            ));
        } else {
            assert!(matches!(
                error,
                MetastoreError::FailedPrecondition {
                    entity: EntityKind::Splits { .. },
                    ..
                }
            ));
        }
        for file in &files {
            assert_eq!(read_file(&metastore, file).await, *file);
        }
        assert_split_states(
            &metastore,
            &index_uid,
            &[
                ("good", SplitState::Staged),
                ("published", SplitState::Published),
                ("marked", SplitState::MarkedForDeletion),
            ],
        )
        .await;
    }
    // Both inputs and the valid output remain usable after every failed transaction.
    assert!(
        metastore
            .publish_file_splits(publication_request(&index_uid, &files, &["good"]))
            .await
            .unwrap()
            .published
    );
    for file in &files {
        assert!(
            read_file(&metastore, file)
                .await
                .completed_timestamp
                .is_some()
        );
    }
    assert_split_states(
        &metastore,
        &index_uid,
        &[
            ("good", SplitState::Published),
            ("published", SplitState::Published),
            ("marked", SplitState::MarkedForDeletion),
        ],
    )
    .await;
    cleanup_index(&mut metastore, index_uid).await;
}

pub async fn test_publish_file_splits_missing_inputs_and_index<
    M: MetastoreServiceExt + DefaultForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let other_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100, 100]).await;
    let foreign_files = register_files(&metastore, &other_uid, &[100]).await;
    stage_splits(&metastore, &index_uid, &["good"]).await;
    stage_splits(&metastore, &other_uid, &["foreign"]).await;
    let publication = publication_request(&index_uid, &files, &["good"]);
    for absent_uid in [
        IndexUid::new_with_random_ulid("missing-file-publication-index"),
        IndexUid::new_with_random_ulid(&index_uid.index_id),
    ] {
        let request = PublishFileSplits {
            index_uid: Some(absent_uid),
            ..publication.clone()
        };
        assert!(matches!(
            metastore.publish_file_splits(request).await.unwrap_err(),
            MetastoreError::NotFound(EntityKind::Index { .. })
        ));
    }
    for absent_file_id in [Ulid::new().to_string(), foreign_files[0].file_id.clone()] {
        let request = PublishFileSplits {
            file_ids: vec![files[0].file_id.clone(), absent_file_id],
            ..publication.clone()
        };
        assert!(matches!(
            metastore.publish_file_splits(request).await.unwrap_err(),
            MetastoreError::FailedPrecondition {
                entity: EntityKind::Index { .. },
                ..
            }
        ));
    }
    // Existing inputs in a different index cannot be completed through this index.
    assert!(matches!(
        metastore
            .publish_file_splits(PublishFileSplits {
                index_uid: Some(other_uid.clone()),
                staged_split_ids: vec!["foreign".to_string()],
                ..publication.clone()
            })
            .await
            .unwrap_err(),
        MetastoreError::FailedPrecondition {
            entity: EntityKind::Index { .. },
            ..
        }
    ));
    assert!(matches!(
        metastore
            .publish_file_splits(PublishFileSplits {
                staged_split_ids: vec!["good".to_string(), "foreign".to_string()],
                ..publication.clone()
            })
            .await
            .unwrap_err(),
        MetastoreError::NotFound(EntityKind::Splits { .. })
    ));
    for file in files.iter().chain(&foreign_files) {
        assert_eq!(read_file(&metastore, file).await, *file);
    }
    assert_split_states(&metastore, &index_uid, &[("good", SplitState::Staged)]).await;
    assert_split_states(&metastore, &other_uid, &[("foreign", SplitState::Staged)]).await;

    // Missing inputs must be detected even when another input already has a terminal outcome.
    let completed = publication_request(&index_uid, &files[..1], &[]);
    assert!(
        metastore
            .publish_file_splits(completed)
            .await
            .unwrap()
            .published
    );
    let request = PublishFileSplits {
        file_ids: vec![
            files[0].file_id.clone(),
            files[1].file_id.clone(),
            Ulid::new().to_string(),
        ],
        ..publication
    };
    assert!(matches!(
        metastore.publish_file_splits(request).await.unwrap_err(),
        MetastoreError::FailedPrecondition {
            entity: EntityKind::Index { .. },
            ..
        }
    ));
    assert_eq!(read_file(&metastore, &files[1]).await, files[1]);
    assert_split_states(&metastore, &index_uid, &[("good", SplitState::Staged)]).await;
    cleanup_index(&mut metastore, index_uid).await;
    cleanup_index(&mut metastore, other_uid).await;
}

pub async fn test_publish_file_splits_invalid_request<M: MetastoreServiceExt + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100, 100]).await;
    stage_splits(&metastore, &index_uid, &["good"]).await;
    let publication = publication_request(&index_uid, &files, &["good"]);
    let invalid_requests = [
        PublishFileSplits {
            index_uid: None,
            ..publication.clone()
        },
        PublishFileSplits {
            index_uid: Some(IndexUid::new_with_random_ulid("")),
            ..publication.clone()
        },
        PublishFileSplits {
            file_ids: vec![],
            ..publication.clone()
        },
        PublishFileSplits {
            file_ids: (0..1001).map(|_| Ulid::new().to_string()).collect(),
            ..publication.clone()
        },
        PublishFileSplits {
            file_ids: vec![files[0].file_id.clone(), files[0].file_id.clone()],
            ..publication.clone()
        },
        PublishFileSplits {
            file_ids: vec!["invalid".to_string()],
            ..publication.clone()
        },
        PublishFileSplits {
            file_ids: vec!["01arz3ndektsv4rrffq69g5fav".to_string()],
            ..publication.clone()
        },
        PublishFileSplits {
            publication_id: String::new(),
            ..publication.clone()
        },
        PublishFileSplits {
            publication_id: "invalid".to_string(),
            ..publication.clone()
        },
        PublishFileSplits {
            publication_id: "01arz3ndektsv4rrffq69g5fav".to_string(),
            ..publication.clone()
        },
        PublishFileSplits {
            staged_split_ids: vec!["good".to_string(), "good".to_string()],
            ..publication.clone()
        },
        PublishFileSplits {
            staged_split_ids: vec![String::new()],
            ..publication.clone()
        },
        PublishFileSplits {
            staged_split_ids: (0..1001).map(|i| format!("split-{i}")).collect(),
            ..publication.clone()
        },
    ];
    for invalid in invalid_requests {
        assert!(matches!(
            metastore.publish_file_splits(invalid).await.unwrap_err(),
            MetastoreError::InvalidArgument { .. }
        ));
        for file in &files {
            assert_eq!(read_file(&metastore, file).await, *file);
        }
        assert_split_states(&metastore, &index_uid, &[("good", SplitState::Staged)]).await;
    }
    assert!(
        metastore
            .publish_file_splits(publication)
            .await
            .unwrap()
            .published
    );
    cleanup_index(&mut metastore, index_uid).await;
}

#[tokio::test]
async fn test_publish_file_splits_validation_grpc() {
    test_publish_file_splits_output_rollback::<GrpcMetastore>().await;
    test_publish_file_splits_missing_inputs_and_index::<GrpcMetastore>().await;
    test_publish_file_splits_invalid_request::<GrpcMetastore>().await;
}
