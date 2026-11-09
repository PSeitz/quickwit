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

use std::collections::BTreeMap;

use quickwit_proto::metastore::{
    DeleteSplitsRequest, ListSplitsRequest, MarkSplitsForDeletionRequest, MetastoreService,
    StageSplitsRequest,
};
use quickwit_proto::types::IndexUid;
use time::OffsetDateTime;

use super::staged_file::{
    GrpcMetastore, WriteStagedFileForTest, assignment_request, create_index, publication_request,
    read_file, register_files,
};
use super::{DefaultForTest, cleanup_index};
use crate::metastore::MetastoreServiceStreamSplitsExt;
use crate::{
    ListSplitsRequestExt, MetastoreServiceExt, SplitMetadata, SplitState, StageSplitsRequestExt,
};

pub(crate) async fn stage_splits(
    metastore: &impl MetastoreService,
    index_uid: &IndexUid,
    split_ids: &[&str],
) {
    for split_id in split_ids {
        let metadata = SplitMetadata {
            split_id: (*split_id).into(),
            index_uid: index_uid.clone(),
            ..Default::default()
        };
        metastore
            .stage_splits(
                StageSplitsRequest::try_from_split_metadata(index_uid.clone(), &metadata).unwrap(),
            )
            .await
            .unwrap();
    }
}

pub(crate) async fn assert_split_states(
    metastore: &impl MetastoreService,
    index_uid: &IndexUid,
    expected: &[(&str, SplitState)],
) {
    let splits = metastore
        .list_splits(ListSplitsRequest::try_from_index_uid(index_uid.clone()).unwrap())
        .await
        .unwrap()
        .collect_splits()
        .await
        .unwrap();
    let actual: BTreeMap<_, _> = splits
        .into_iter()
        .map(|split| (split.split_id().to_string(), split.split_state))
        .collect();
    let expected: BTreeMap<_, _> = expected
        .iter()
        .map(|(split_id, state)| (split_id.to_string(), *state))
        .collect();
    assert_eq!(actual, expected);
}

pub async fn test_publish_file_splits_atomic_and_retry<M: MetastoreServiceExt + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    register_files(&metastore, &index_uid, &[100, 200]).await;
    let files = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    assert_eq!(files.len(), 2);
    stage_splits(&metastore, &index_uid, &["first", "second", "unused"]).await;
    let publication = publication_request(&index_uid, &files, &["first", "second"]);
    let before = OffsetDateTime::now_utc().unix_timestamp();
    // Commit but ignore the response, as if the connection had been lost.
    metastore
        .publish_file_splits(publication.clone())
        .await
        .unwrap();
    let after = OffsetDateTime::now_utc().unix_timestamp();
    let mut completed = Vec::new();
    for file in &files {
        let stored = read_file(&metastore, file).await;
        assert!((before..=after).contains(&stored.completed_timestamp.unwrap()));
        let mut expected = file.clone();
        expected.completed_timestamp = stored.completed_timestamp;
        expected.publication_id = Some(publication.publication_id.clone());
        assert_eq!(stored, expected);
        completed.push(stored);
    }
    assert_eq!(
        completed[0].completed_timestamp,
        completed[1].completed_timestamp
    );
    assert_split_states(
        &metastore,
        &index_uid,
        &[
            ("first", SplitState::Published),
            ("second", SplitState::Published),
            ("unused", SplitState::Staged),
        ],
    )
    .await;
    assert!(
        metastore
            .publish_file_splits(publication.clone())
            .await
            .unwrap()
            .published
    );

    // Completion rows retain the answer after the output splits have been garbage-collected.
    metastore
        .mark_splits_for_deletion(MarkSplitsForDeletionRequest::new(
            index_uid.clone(),
            publication.staged_split_ids.clone(),
        ))
        .await
        .unwrap();
    metastore
        .delete_splits(DeleteSplitsRequest {
            index_uid: Some(index_uid.clone()),
            split_ids: publication.staged_split_ids.clone(),
        })
        .await
        .unwrap();
    assert!(
        metastore
            .publish_file_splits(publication.clone())
            .await
            .unwrap()
            .published
    );
    // A competing attempt loses before output validation, including when its outputs are absent.
    let loser = publication_request(&index_uid, &files, &["never-staged"]);
    assert!(
        !metastore
            .publish_file_splits(loser)
            .await
            .unwrap()
            .published
    );
    for file in &completed {
        assert_eq!(read_file(&metastore, file).await, *file);
    }
    assert_split_states(&metastore, &index_uid, &[("unused", SplitState::Staged)]).await;
    cleanup_index(&mut metastore, index_uid).await;
}

pub async fn test_publish_file_splits_zero_outputs<M: MetastoreServiceExt + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100, 200]).await;
    // Assignment ownership is not a publication precondition. Filtering can produce no splits.
    let publication = publication_request(&index_uid, &files, &[]);
    assert!(
        metastore
            .publish_file_splits(publication.clone())
            .await
            .unwrap()
            .published
    );
    for file in &files {
        let stored = read_file(&metastore, file).await;
        assert!(stored.completed_timestamp.is_some());
        assert_eq!(
            stored.publication_id.as_ref(),
            Some(&publication.publication_id)
        );
        assert_eq!(stored.processing_started_timestamp, None);
    }
    assert!(
        metastore
            .publish_file_splits(publication)
            .await
            .unwrap()
            .published
    );
    assert!(
        metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    assert_split_states(&metastore, &index_uid, &[]).await;
    cleanup_index(&mut metastore, index_uid).await;
}

pub async fn test_publish_file_splits_older_attempt<
    M: MetastoreServiceExt + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    register_files(&metastore, &index_uid, &[100, 100]).await;
    let first = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    assert_eq!(first.len(), 2);
    let older_attempt = publication_request(&index_uid, &first, &["older"]);
    for file in &first {
        let mut expired = file.clone();
        expired.processing_started_timestamp = Some(1);
        metastore.set_staged_file(expired).await;
    }
    let second = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    assert_eq!(second.len(), 2);
    for file in &second {
        assert!(file.processing_started_timestamp.unwrap() > 1);
    }
    let newer_attempt = publication_request(&index_uid, &second, &["newer"]);
    stage_splits(&metastore, &index_uid, &["older", "newer"]).await;
    assert!(
        metastore
            .publish_file_splits(older_attempt.clone())
            .await
            .unwrap()
            .published
    );
    assert!(
        !metastore
            .publish_file_splits(newer_attempt)
            .await
            .unwrap()
            .published
    );
    for file in &second {
        let stored = read_file(&metastore, file).await;
        assert_eq!(
            stored.publication_id.as_ref(),
            Some(&older_attempt.publication_id)
        );
        assert!(stored.completed_timestamp.is_some());
        assert_eq!(
            stored.processing_started_timestamp,
            file.processing_started_timestamp
        );
    }
    assert_split_states(
        &metastore,
        &index_uid,
        &[
            ("older", SplitState::Published),
            ("newer", SplitState::Staged),
        ],
    )
    .await;
    cleanup_index(&mut metastore, index_uid).await;
}

pub async fn test_publish_file_splits_overlap<M: MetastoreServiceExt + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100, 100, 100]).await;
    stage_splits(&metastore, &index_uid, &["winner", "loser", "remaining"]).await;
    let winner = publication_request(&index_uid, &files[..2], &["winner"]);
    let overlap = publication_request(&index_uid, &files[1..], &["loser"]);
    assert!(
        metastore
            .publish_file_splits(winner.clone())
            .await
            .unwrap()
            .published
    );
    let first = read_file(&metastore, &files[0]).await;
    let second = read_file(&metastore, &files[1]).await;
    assert!(
        !metastore
            .publish_file_splits(overlap.clone())
            .await
            .unwrap()
            .published
    );
    assert_eq!(read_file(&metastore, &files[0]).await, first);
    assert_eq!(read_file(&metastore, &files[1]).await, second);
    assert_eq!(read_file(&metastore, &files[2]).await, files[2]);
    assert_split_states(
        &metastore,
        &index_uid,
        &[
            ("winner", SplitState::Published),
            ("loser", SplitState::Staged),
            ("remaining", SplitState::Staged),
        ],
    )
    .await;
    let remaining = publication_request(&index_uid, &files[2..], &["remaining"]);
    assert!(
        metastore
            .publish_file_splits(remaining)
            .await
            .unwrap()
            .published
    );
    // All inputs are now completed, but by different publications. The overlapping attempt loses.
    assert!(
        !metastore
            .publish_file_splits(overlap)
            .await
            .unwrap()
            .published
    );
    assert!(
        metastore
            .publish_file_splits(winner)
            .await
            .unwrap()
            .published
    );
    assert_split_states(
        &metastore,
        &index_uid,
        &[
            ("winner", SplitState::Published),
            ("loser", SplitState::Staged),
            ("remaining", SplitState::Published),
        ],
    )
    .await;
    cleanup_index(&mut metastore, index_uid).await;
}

pub async fn test_publish_file_splits_concurrent<M: MetastoreServiceExt + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    for overlapping in [false, true] {
        let index_uid = create_index(&mut metastore).await;
        let files = register_files(&metastore, &index_uid, &[100, 100, 100]).await;
        stage_splits(
            &metastore,
            &index_uid,
            &["left-1", "left-2", "right-1", "right-2"],
        )
        .await;
        let left = publication_request(&index_uid, &files[..2], &["left-1", "left-2"]);
        // Reverse input order to exercise lock ordering for identical and overlapping groups.
        let right_files = if overlapping {
            vec![files[2].clone(), files[1].clone()]
        } else {
            vec![files[1].clone(), files[0].clone()]
        };
        let right = publication_request(&index_uid, &right_files, &["right-2", "right-1"]);
        let (left_result, right_result) = tokio::join!(
            metastore.publish_file_splits(left.clone()),
            metastore.publish_file_splits(right.clone()),
        );
        let left_won = left_result.unwrap().published;
        assert_ne!(left_won, right_result.unwrap().published);
        let (winner, loser) = if left_won {
            (&left, &right)
        } else {
            (&right, &left)
        };
        for file in &files {
            let stored = read_file(&metastore, file).await;
            if winner.file_ids.contains(&file.file_id) {
                assert_eq!(stored.publication_id.as_ref(), Some(&winner.publication_id));
                assert!(stored.completed_timestamp.is_some());
            } else {
                assert_eq!(stored, *file);
            }
        }
        let left_state = if left_won {
            SplitState::Published
        } else {
            SplitState::Staged
        };
        let right_state = if left_won {
            SplitState::Staged
        } else {
            SplitState::Published
        };
        assert_split_states(
            &metastore,
            &index_uid,
            &[
                ("left-1", left_state),
                ("left-2", left_state),
                ("right-1", right_state),
                ("right-2", right_state),
            ],
        )
        .await;
        assert!(
            metastore
                .publish_file_splits(winner.clone())
                .await
                .unwrap()
                .published
        );
        assert!(
            !metastore
                .publish_file_splits(loser.clone())
                .await
                .unwrap()
                .published
        );
        cleanup_index(&mut metastore, index_uid).await;
    }
}

#[tokio::test]
async fn test_publish_file_splits_grpc() {
    test_publish_file_splits_atomic_and_retry::<GrpcMetastore>().await;
    test_publish_file_splits_zero_outputs::<GrpcMetastore>().await;
    test_publish_file_splits_overlap::<GrpcMetastore>().await;
    test_publish_file_splits_concurrent::<GrpcMetastore>().await;
}
