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

use anyhow::Context;
use quickwit_common::Progress;
use quickwit_common::uri::Uri;
use quickwit_proto::metastore::{
    ListCompletedFilesRequest, MetastoreService, MetastoreServiceClient,
};
use quickwit_storage::StorageResolver;

/// Keep completion evidence and restart each scan: recovery may re-upload a completed
/// input and crash before registration. Completions behind the cursor wait for the next pass.
pub(crate) async fn cleanup_completed_files(
    metastore: &MetastoreServiceClient,
    storage_resolver: &StorageResolver,
    progress: &Progress,
) -> anyhow::Result<()> {
    let mut after_file_id = String::new();
    let mut result = Ok(());
    loop {
        let response = progress
            .protect_future(metastore.list_completed_files(ListCompletedFilesRequest {
                after_file_id: after_file_id.clone(),
                max_num_files: 1_000,
            }))
            .await?;
        if response.staged_files.is_empty() {
            return result;
        }
        for file in response.staged_files {
            // Validate the listing contract before any destructive operation.
            anyhow::ensure!(
                file.completed_timestamp.is_some() && file.file_id > after_file_id,
                "invalid completed-file listing at {}",
                file.file_id
            );
            after_file_id = file.file_id;
            if let Err(error) = progress
                .protect_future(delete_input(storage_resolver, &file.file_uri))
                .await
            {
                tracing::warn!(file_id = %after_file_id, ?error, "failed to delete completed input; retrying next GC pass");
                result = Err(error);
            }
        }
    }
}

async fn delete_input(storage_resolver: &StorageResolver, file_uri: &str) -> anyhow::Result<()> {
    let uri: Uri = file_uri.parse()?;
    let parent = uri.parent().context("input URI has no parent")?;
    let filename = uri.file_name().context("input URI has no filename")?;
    // Storage::delete already treats an absent object as success.
    storage_resolver
        .resolve(&parent)
        .await?
        .delete(filename)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use quickwit_proto::metastore::{
        DocumentsStagedForUpload, ListCompletedFilesResponse, MockMetastoreService,
    };

    use super::*;

    #[tokio::test]
    async fn test_completed_file_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // A directory forces a real delete failure. Later files must still be processed.
        std::fs::create_dir(root.join("a")).unwrap();
        for name in ["c", "unfinished", "output.split"] {
            std::fs::write(root.join(name), b"input").unwrap();
        }
        let file_uri = |name| format!("file://{}/{name}", root.display());
        let files: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|name| DocumentsStagedForUpload {
                file_id: name.to_string(),
                file_uri: file_uri(name),
                completed_timestamp: Some(1),
                ..Default::default()
            })
            .collect();
        let mut mock = MockMetastoreService::new();
        mock.expect_list_completed_files()
            .returning(move |request| {
                // One record per page exercises pagination without a large fixture.
                Ok(ListCompletedFilesResponse {
                    staged_files: files
                        .iter()
                        .filter(|file| file.file_id > request.after_file_id)
                        .take(1)
                        .cloned()
                        .collect(),
                })
            });
        let metastore = MetastoreServiceClient::from_mock(mock);
        let resolver = StorageResolver::for_test();
        let progress = Progress::default();
        assert!(
            cleanup_completed_files(&metastore, &resolver, &progress)
                .await
                .is_err()
        );
        assert!(root.join("a").is_dir());
        assert!(!root.join("b").try_exists().unwrap());
        assert!(!root.join("c").try_exists().unwrap());
        std::fs::remove_dir(root.join("a")).unwrap();
        // Retry and then recreate a completed input, as recovery can do before registration.
        for _ in 0..2 {
            std::fs::write(root.join("a"), b"input").unwrap();
            cleanup_completed_files(&metastore, &resolver, &progress)
                .await
                .unwrap();
            assert!(!root.join("a").try_exists().unwrap());
            assert!(root.join("unfinished").try_exists().unwrap());
            assert!(root.join("output.split").try_exists().unwrap());
        }
        // Even an invalid listing response must preserve unfinished input.
        let mut mock = MockMetastoreService::new();
        let unfinished = DocumentsStagedForUpload {
            file_id: "unfinished".to_string(),
            file_uri: file_uri("unfinished"),
            ..Default::default()
        };
        mock.expect_list_completed_files().return_once(|_| {
            Ok(ListCompletedFilesResponse {
                staged_files: vec![unfinished],
            })
        });
        let metastore = MetastoreServiceClient::from_mock(mock);
        assert!(
            cleanup_completed_files(&metastore, &resolver, &progress)
                .await
                .is_err()
        );
        assert!(root.join("unfinished").try_exists().unwrap());
    }
}
