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

use std::collections::BTreeSet;

use quickwit_proto::metastore::{
    AssignFilesForIndexing, DocumentsStagedForUpload, MetastoreError, MetastoreService,
};
use time::OffsetDateTime;

use super::DefaultForTest;
use super::staged_file::{
    GrpcMetastore, WriteStagedFileForTest, assignment_request, create_index, delete_index,
    publication_request, read_file, register_files,
};

fn file_ids(files: &[DocumentsStagedForUpload]) -> Vec<&str> {
    files.iter().map(|file| file.file_id.as_str()).collect()
}

pub async fn test_assign_files_oldest_index_group<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let first_index = create_index(&mut metastore).await;
    let second_index = create_index(&mut metastore).await;
    // Registration order differs from creation order. Equal timestamps use file IDs.
    let mut second = register_files(&metastore, &second_index, &[100]).await;
    let mut first = register_files(&metastore, &first_index, &[100, 100, 100]).await;
    first[..2].sort_by(|left, right| left.file_id.cmp(&right.file_id));
    for (file, timestamp) in first.iter_mut().zip([10, 10, 12]) {
        file.create_timestamp = timestamp;
        metastore.set_staged_file(file.clone()).await;
    }
    second[0].create_timestamp = 11;
    metastore.set_staged_file(second[0].clone()).await;

    let before = OffsetDateTime::now_utc().unix_timestamp();
    let assigned = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    let after = OffsetDateTime::now_utc().unix_timestamp();
    assert_eq!(file_ids(&assigned), file_ids(&first));
    for file in &assigned {
        assert_eq!(file.index_uid.as_ref(), Some(&first_index));
        assert!((before..=after).contains(&file.processing_started_timestamp.unwrap()));
        assert_eq!(
            file.processing_started_timestamp,
            assigned[0].processing_started_timestamp
        );
        assert_eq!(read_file(&metastore, file).await, *file);
    }
    // The other index's file waits even though it is older than the group's last file.
    let assigned = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    assert_eq!(file_ids(&assigned), file_ids(&second));

    // The same timestamp across indexes is also ordered by file ID.
    let mut tied = register_files(&metastore, &first_index, &[100]).await;
    tied.extend(register_files(&metastore, &second_index, &[100]).await);
    tied.sort_by(|left, right| left.file_id.cmp(&right.file_id));
    for file in &mut tied {
        file.create_timestamp = 10;
        metastore.set_staged_file(file.clone()).await;
    }
    for file in &tied {
        let assigned = metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap()
            .staged_files;
        assert_eq!(file_ids(&assigned), vec![file.file_id.as_str()]);
    }
    assert!(
        metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    delete_index(&mut metastore, first_index).await;
    delete_index(&mut metastore, second_index).await;
}

pub async fn test_assign_files_count_and_byte_prefix<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    // Each expected group is a prefix. A large intervening file cannot be skipped.
    for (sizes, max_num_files, max_num_bytes, group_lengths) in [
        (vec![40, 70, 10], 3, 100, vec![1, 2]),
        (vec![40, 20, 10], 2, 100, vec![2, 1]),
        (vec![40, 60, 10], 3, 100, vec![2, 1]),
        (vec![150, 10], 3, 100, vec![1, 1]),
        (
            vec![i64::MAX as u64, i64::MAX as u64],
            2,
            i64::MAX as u64,
            vec![1, 1],
        ),
    ] {
        let index_uid = create_index(&mut metastore).await;
        let mut files = register_files(&metastore, &index_uid, &sizes).await;
        for (i, file) in files.iter_mut().enumerate() {
            file.create_timestamp = 10 + i as i64;
            metastore.set_staged_file(file.clone()).await;
        }
        let request = AssignFilesForIndexing {
            max_num_files,
            max_num_bytes,
            ..assignment_request()
        };
        let mut offset = 0;
        for count in group_lengths {
            let assigned = metastore
                .assign_files_for_indexing(request)
                .await
                .unwrap()
                .staged_files;
            assert_eq!(
                file_ids(&assigned),
                file_ids(&files[offset..offset + count])
            );
            offset += count;
        }
        assert_eq!(offset, files.len());
        assert!(
            metastore
                .assign_files_for_indexing(request)
                .await
                .unwrap()
                .staged_files
                .is_empty()
        );
        delete_index(&mut metastore, index_uid).await;
    }
}

pub async fn test_assign_files_lost_response_and_expiry<
    M: MetastoreService + DefaultForTest + WriteStagedFileForTest,
>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let mut files = register_files(&metastore, &index_uid, &[100, 100, 100]).await;
    for (i, file) in files.iter_mut().enumerate() {
        file.create_timestamp = 10 + i as i64;
        metastore.set_staged_file(file.clone()).await;
    }
    let request = AssignFilesForIndexing {
        max_num_files: 1,
        ..assignment_request()
    };
    // Lose the first response. A fresh request must leave that current assignment alone.
    metastore.assign_files_for_indexing(request).await.unwrap();
    let second = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(file_ids(&second), file_ids(&files[1..2]));
    assert!(
        metastore
            .publish_file_splits(publication_request(&index_uid, &second, &[]))
            .await
            .unwrap()
            .published
    );
    let third = metastore
        .assign_files_for_indexing(request)
        .await
        .unwrap()
        .staged_files;
    assert_eq!(file_ids(&third), file_ids(&files[2..]));
    assert!(
        metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );

    // Advance expiry deterministically. Preserve the completion evidence on the second input.
    for file in &files[..2] {
        let mut stored = read_file(&metastore, file).await;
        assert!(stored.processing_started_timestamp.is_some());
        stored.processing_started_timestamp = Some(1);
        metastore.set_staged_file(stored).await;
    }
    let completed = read_file(&metastore, &files[1]).await;
    let assigned = metastore
        .assign_files_for_indexing(assignment_request())
        .await
        .unwrap()
        .staged_files;
    assert_eq!(file_ids(&assigned), file_ids(&files[..1]));
    assert!(assigned[0].processing_started_timestamp.unwrap() > 1);
    assert_eq!(assigned[0].completed_timestamp, None);
    assert_eq!(assigned[0].publication_id, None);
    assert_eq!(read_file(&metastore, &files[1]).await, completed);
    assert_eq!(read_file(&metastore, &files[2]).await, third[0]);
    assert!(
        metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_assign_files_concurrent<M: MetastoreService + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100; 16]).await;
    let request = AssignFilesForIndexing {
        max_num_files: 1,
        ..assignment_request()
    };
    let requests = (0..16).map(|_| metastore.assign_files_for_indexing(request));
    let replies = futures::future::join_all(requests).await;
    let mut assigned_ids = BTreeSet::new();
    for reply in replies {
        let assigned = reply.unwrap().staged_files;
        assert_eq!(assigned.len(), 1);
        assert!(assigned_ids.insert(assigned[0].file_id.clone()));
        assert!(assigned[0].processing_started_timestamp.is_some());
        assert_eq!(read_file(&metastore, &assigned[0]).await, assigned[0]);
    }
    assert_eq!(
        assigned_ids,
        files.iter().map(|file| file.file_id.clone()).collect()
    );
    assert!(
        metastore
            .assign_files_for_indexing(request)
            .await
            .unwrap()
            .staged_files
            .is_empty()
    );
    delete_index(&mut metastore, index_uid).await;
}

pub async fn test_assign_files_invalid_request<M: MetastoreService + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let index_uid = create_index(&mut metastore).await;
    let files = register_files(&metastore, &index_uid, &[100]).await;
    for invalid in [
        AssignFilesForIndexing {
            max_num_files: 0,
            ..assignment_request()
        },
        AssignFilesForIndexing {
            max_num_files: 1001,
            ..assignment_request()
        },
        AssignFilesForIndexing {
            max_num_bytes: 0,
            ..assignment_request()
        },
        AssignFilesForIndexing {
            max_num_bytes: i64::MAX as u64 + 1,
            ..assignment_request()
        },
        AssignFilesForIndexing {
            retry_after_secs: 0,
            ..assignment_request()
        },
    ] {
        assert!(matches!(
            metastore
                .assign_files_for_indexing(invalid)
                .await
                .unwrap_err(),
            MetastoreError::InvalidArgument { .. }
        ));
        assert_eq!(read_file(&metastore, &files[0]).await, files[0]);
    }
    assert_eq!(
        metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap()
            .staged_files
            .len(),
        1
    );
    delete_index(&mut metastore, index_uid).await;
}

#[tokio::test]
async fn test_assign_files_grpc() {
    test_assign_files_concurrent::<GrpcMetastore>().await;
    test_assign_files_invalid_request::<GrpcMetastore>().await;
}
