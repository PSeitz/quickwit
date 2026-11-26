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

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use quickwit_common::retry::Retryable;
use quickwit_proto::metastore::{
    EntityKind, MetastoreError, MetastoreService, MetastoreServiceClient, RegisterFileForIndexing,
};
use quickwit_storage::{FilePayload, Storage, StorageErrorKind};
use tokio::time::Instant;
use ulid::Ulid;

use super::DocumentsStagedForUpload;

const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Tracks uploads, registration, and local deletion of finished files.
/// Each file retries independently.
#[derive(Default)]
pub(super) struct FileHandoffs {
    // IDs of files scheduled for upload, retained until local deletion.
    scheduled_file_ids: HashSet<Ulid>,
    uploads: FuturesUnordered<BoxFuture<'static, anyhow::Result<Ulid>>>,
    pending_delete: BTreeSet<(Instant, Ulid)>,
}

impl FileHandoffs {
    /// Schedules uploads for finished files. Repeated scans schedule each file once
    /// while its ID is tracked. `next_delete` polls the uploads and registration.
    pub fn schedule_uploads(
        &mut self,
        uploader: &Uploader,
        finished_files: &[DocumentsStagedForUpload],
    ) {
        for file in finished_files {
            if self.scheduled_file_ids.insert(file.file_id) {
                self.uploads.push(uploader.start(file));
            }
        }
    }

    /// Polls uploads and registration before yielding a file ID for local deletion.
    /// Upload futures and deletion deadlines remain in `self` when the writer's
    /// select loop cancels this call.
    pub async fn next_delete(&mut self) -> anyhow::Result<Ulid> {
        loop {
            let deadline = self.pending_delete.first().map(|(deadline, _)| *deadline);
            tokio::select! {
                biased;
                Some(result) = self.uploads.next(), if !self.uploads.is_empty() => {
                    self.pending_delete.insert((Instant::now(), result?));
                }
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let (_, file_id) = self.pending_delete.pop_first().expect("deletion is pending");
                    return Ok(file_id);
                }
            }
        }
    }

    pub fn deleted(&mut self, file_id: Ulid) {
        assert!(self.scheduled_file_ids.remove(&file_id));
    }

    pub fn retry_delete(&mut self, file_id: Ulid) {
        self.pending_delete
            .insert((Instant::now() + RETRY_DELAY, file_id));
    }
}

/// Uploads finished files and registers them for indexing.
///
/// The storage must be resolved at the upload prefix. Keep this destination
/// unchanged across restarts while local files remain: recovery retains file IDs,
/// but the local format does not store their remote URIs.
pub struct Uploader {
    storage: Arc<dyn Storage>,
    metastore: MetastoreServiceClient,
}

impl Uploader {
    pub fn new(storage: Arc<dyn Storage>, metastore: MetastoreServiceClient) -> Self {
        Self { storage, metastore }
    }

    /// Completes with the file ID once registered, or discarded because its index is gone.
    /// The writer can then delete the local copy and release its disk charge.
    pub(super) fn start(
        &self,
        input: &DocumentsStagedForUpload,
    ) -> BoxFuture<'static, anyhow::Result<Ulid>> {
        let input = input.clone();
        let storage = self.storage.clone();
        let metastore = self.metastore.clone();
        Box::pin(async move {
            let file_id = input.file_id;
            let request = RegisterFileForIndexing {
                index_uid: Some(input.index_uid.clone()),
                file_id: file_id.to_string(),
                file_uri: storage.uri().join(file_id.to_string())?.into_string(),
                num_bytes: input.num_bytes,
            };
            let object_path = PathBuf::from(file_id.to_string());
            let payload = FilePayload::new(input.path.clone(), input.num_bytes);
            // An error can follow a committed upload. Replay the same bytes at the same key.
            while let Err(error) = storage.put(&object_path, Box::new(payload.clone())).await {
                if matches!(
                    error.kind(),
                    StorageErrorKind::Unauthorized | StorageErrorKind::NotFound
                ) {
                    return Err(error).with_context(|| format!("failed to upload file {file_id}"));
                }
                tracing::warn!(%file_id, ?error, "failed to upload ingesthree file; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
            // Once upload succeeds, retries use only the metastore. This also reconciles a
            // lost registration response without resetting processing/completion timestamps.
            while let Err(error) = metastore.register_file_for_indexing(request.clone()).await {
                if matches!(error, MetastoreError::NotFound(EntityKind::Index { .. })) {
                    tracing::error!(%file_id, index_uid = %input.index_uid, "discarding ingesthree file because its index no longer exists");
                    // Registration checks the exact incarnation. Never redirect old input to
                    // a recreated index. Remove the uploaded object before releasing local data.
                    while let Err(error) = storage.delete(&object_path).await {
                        if error.kind() == StorageErrorKind::NotFound {
                            break;
                        }
                        tracing::warn!(%file_id, ?error, "failed to delete discarded ingesthree object; retrying");
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                    return Ok(file_id);
                }
                if !error.is_retryable() && !matches!(error, MetastoreError::TooManyRequests) {
                    return Err(error)
                        .with_context(|| format!("failed to register file {file_id}"));
                }
                tracing::warn!(%file_id, ?error, "failed to register ingesthree file; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Ok(file_id)
        })
    }
}
