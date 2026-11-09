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
    DocumentsStagedForUpload, ListCompletedFilesRequest, MetastoreError, MetastoreService,
};

use super::DefaultForTest;
use super::staged_file::{
    GrpcMetastore, assignment_request, create_index, delete_index, publication_request, read_file,
    register_files,
};

async fn list(
    metastore: &impl MetastoreService,
    after: &str,
    limit: u32,
) -> Vec<DocumentsStagedForUpload> {
    metastore
        .list_completed_files(ListCompletedFilesRequest {
            after_file_id: after.to_string(),
            max_num_files: limit,
        })
        .await
        .unwrap()
        .staged_files
}

pub async fn test_list_completed_files<M: MetastoreService + DefaultForTest>() {
    let mut metastore = M::default_for_test().await;
    let indexes = [
        create_index(&mut metastore).await,
        create_index(&mut metastore).await,
    ];
    assert!(list(&metastore, "", 2).await.is_empty());
    let mut files = Vec::new();
    for i in 0..6 {
        files.extend(register_files(&metastore, &indexes[i % 2], &[100]).await);
    }
    files.sort_unstable_by(|left, right| left.file_id.cmp(&right.file_id));
    for _ in 0..2 {
        metastore
            .assign_files_for_indexing(assignment_request())
            .await
            .unwrap();
    }
    // Exclude both assigned and never-assigned unfinished inputs.
    let pending = register_files(&metastore, &indexes[0], &[100])
        .await
        .pop()
        .unwrap();
    let mut completed = Vec::new();
    // Scan three completions, then repeat after a completion behind the old cursor.
    for file_indices in [&[1, 2, 5][..], &[0][..]] {
        for &i in file_indices {
            let file = &files[i];
            let request = publication_request(file.index_uid(), std::slice::from_ref(file), &[]);
            assert!(
                metastore
                    .publish_file_splits(request)
                    .await
                    .unwrap()
                    .published
            );
            completed.push(read_file(&metastore, file).await);
        }
        completed.sort_unstable_by(|left, right| left.file_id.cmp(&right.file_id));
        assert_eq!(list(&metastore, "", 2).await, completed[..2]);
        assert_eq!(
            list(&metastore, &completed[1].file_id, 2).await,
            completed[2..]
        );
        assert!(list(&metastore, &files[5].file_id, 2).await.is_empty());
        assert_eq!(
            list(&metastore, &files[3].file_id, 1).await,
            completed[completed.len() - 1..]
        );
        assert_eq!(list(&metastore, "", 1000).await, completed);
        // Listing leaves completion evidence intact, including across registration retries.
        for file in &completed {
            assert_eq!(read_file(&metastore, file).await, *file);
        }
    }
    assert_eq!(read_file(&metastore, &pending).await, pending);
    assert!(
        read_file(&metastore, &files[3])
            .await
            .completed_timestamp
            .is_none()
    );
    for index in indexes {
        delete_index(&mut metastore, index).await;
    }
}

pub async fn test_list_completed_files_validation<M: MetastoreService + DefaultForTest>() {
    let metastore = M::default_for_test().await;
    for max_num_files in [0, 1001, u32::MAX] {
        let result = metastore
            .list_completed_files(ListCompletedFilesRequest {
                max_num_files,
                ..Default::default()
            })
            .await;
        assert!(matches!(
            result,
            Err(MetastoreError::InvalidArgument { .. })
        ));
    }
    assert!(list(&metastore, "after-all-ulids", 1).await.is_empty());
}

#[tokio::test]
async fn test_list_completed_files_grpc() {
    test_list_completed_files::<GrpcMetastore>().await;
    test_list_completed_files_validation::<GrpcMetastore>().await;
}
