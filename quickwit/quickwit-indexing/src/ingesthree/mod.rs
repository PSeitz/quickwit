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

//! Indexes assigned files using the v2 split operations.
//! The indexing service owns polling and supervision. Standalone compactors discover published
//! splits in the metastore.

mod indexing;
mod metrics;
mod supervisor;
pub(crate) use supervisor::S3IndexerSupervisor;
pub use supervisor::StopS3Indexers;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use quickwit_common::KillSwitch;
use quickwit_common::io::{IoControls, Limiter};
use quickwit_common::retry::Retryable;
use quickwit_common::runtimes::RuntimeType;
use quickwit_common::temp_dir::TempDirectory;
use quickwit_common::uri::Uri;
use quickwit_metastore::{IndexMetadataResponseExt, SplitMetadata, StageSplitsRequestExt};
use quickwit_metrics::{Gauge, GaugeGuard};
use quickwit_proto::indexing::IndexingPipelineId;
use quickwit_proto::metastore::{
    AssignFilesForIndexing, IndexMetadataRequest, LastDeleteOpstampRequest, MetastoreError,
    MetastoreService, MetastoreServiceClient, PublishFileSplits, StageSplitsRequest,
};
use quickwit_proto::types::{IndexUid, NodeId, PipelineUid, SourceId};
use quickwit_storage::{StorageError, StorageErrorKind, StorageResolver};
use tracing::{Instrument, warn};
use ulid::Ulid;

use self::indexing::{PreparedSplit, SplitBuilders};
use self::metrics::ACTIVE_ASSIGNMENTS;
use crate::actors::acquire_indexing_upload_permit;
use crate::docs_clustering::Fingerprinter;
use crate::merge_policy::merge_policy_from_settings;
use crate::metrics::record_published_split;
use crate::{IndexingSplitCache, IndexingSplitStore};

/// Indexes assigned input files from S3 and publishes their output splits.
/// Other storage backends remain usable through the storage resolver.
/// Shared caches and the upload limit should match the other indexers on this node.
/// The caller polls `index_next`, backs off on an empty assignment, and supervises failures.
#[derive(Clone)]
pub struct S3Indexer {
    pub node_id: NodeId,
    /// Labels split metadata and metrics. This source uses no v2 checkpoints or shards.
    pub source_id: SourceId,
    pub metastore: MetastoreServiceClient,
    pub storage_resolver: StorageResolver,
    pub scratch_directory: TempDirectory,
    pub split_cache: Arc<IndexingSplitCache>,
    pub max_concurrent_split_uploads: usize,
    pub indexing_io_throughput_limiter: Option<Limiter>,
    pub fingerprinter: Option<Fingerprinter>,
}

/// A definite publication outcome. Only a winner returns published split metadata.
/// Remote inputs and losing outputs remain available for the cleanup service.
#[derive(Debug)]
pub struct FilePublication {
    pub index_uid: IndexUid,
    pub published: bool,
    pub splits: Vec<SplitMetadata>,
}

impl S3Indexer {
    /// Claims and indexes a same-index group. Empty assignments return `None`.
    ///
    /// A mapping and delete-opstamp snapshot is taken before processing the group. Invalid JSON
    /// documents are counted/logged and skipped, as in v2. File corruption fails the group.
    /// Cancellation leaves assignment/completion in the metastore and retains remote objects.
    /// Standalone compactors discover confirmed publications directly from the metastore.
    pub async fn index_next(
        &mut self,
        assignment: AssignFilesForIndexing,
    ) -> anyhow::Result<Option<FilePublication>> {
        self.index_next_with_activity(assignment, &ACTIVE_ASSIGNMENTS)
            .await
    }

    async fn index_next_with_activity(
        &mut self,
        assignment: AssignFilesForIndexing,
        active_assignments: &Gauge,
    ) -> anyhow::Result<Option<FilePublication>> {
        ensure!(
            self.max_concurrent_split_uploads > 0,
            "split upload limit must be positive"
        );
        let files = self
            .metastore
            .assign_files_for_indexing(assignment)
            .await?
            .staged_files;
        let Some(first) = files.first() else {
            return Ok(None);
        };
        // The group stays active until its caller and any detached blocking work have exited.
        let active_assignment = Arc::new(GaugeGuard::new(active_assignments, 1.0));
        let index_uid = first
            .index_uid
            .clone()
            .context("assigned file has no index UID")?;
        ensure!(
            files
                .iter()
                .all(|file| file.index_uid.as_ref() == Some(&index_uid)),
            "assigned files span index incarnations"
        );
        let config = self
            .metastore
            .index_metadata(IndexMetadataRequest {
                index_uid: Some(index_uid.clone()),
                ..Default::default()
            })
            .await?
            .deserialize_index_metadata()?
            .index_config;
        let delete_opstamp = self
            .metastore
            .last_delete_opstamp(LastDeleteOpstampRequest {
                index_uid: Some(index_uid.clone()),
            })
            .await?
            .last_delete_opstamp;
        let split_storage = self.storage_resolver.resolve(&config.index_uri).await?;
        let split_store = IndexingSplitStore::new(split_storage, self.split_cache.clone());
        let merge_policy = merge_policy_from_settings(&config.indexing_settings);
        let retention = config.retention_policy_opt.clone();
        let cancellation = StopIndexingOnDrop(KillSwitch::default());
        let io_controls = IoControls::default()
            .set_kill_switch(cancellation.0.clone())
            .set_throughput_limiter_opt(self.indexing_io_throughput_limiter.clone());
        let pipeline_id = IndexingPipelineId {
            node_id: self.node_id.clone(),
            source_id: self.source_id.clone(),
            index_uid: index_uid.clone(),
            pipeline_uid: PipelineUid::random(),
        };
        let scratch_directory = self.scratch_directory.clone();
        let fingerprinter = self.fingerprinter.clone();
        let (directory, mut builders) = run_indexing(
            scratch_directory.clone(),
            active_assignment.clone(),
            move || {
                let directory = scratch_directory.named_temp_child("files-")?;
                let builders = SplitBuilders::new(
                    config,
                    pipeline_id,
                    delete_opstamp,
                    directory.clone(),
                    io_controls,
                    fingerprinter,
                )?;
                Ok((directory, builders))
            },
        )
        .await?;
        for (file_index, file) in files.iter().enumerate() {
            let uri: Uri = file.file_uri.parse()?;
            let storage = self
                .storage_resolver
                .resolve(&uri.parent().context("input URI has no parent")?)
                .await?;
            // Download the entire assignment before dispatching its indexing task.
            // Local names are independent of remote paths and unique within this attempt.
            let path = directory.path().join(file_index.to_string());
            let size = storage
                .copy_to_file(uri.file_name().context("input URI has no filename")?, &path)
                .await?;
            ensure!(
                size == file.num_bytes,
                "downloaded input size differs from registration"
            );
        }
        let files_to_index = files.clone();
        let input_directory = directory.clone();
        let prepared = run_indexing(directory.clone(), active_assignment.clone(), move || {
            let mut prepared = Vec::new();
            for (file_index, file) in files_to_index.iter().enumerate() {
                let path = input_directory.path().join(file_index.to_string());
                prepared.extend(builders.index_file(
                    &path,
                    file,
                    &merge_policy,
                    retention.as_ref(),
                    file_index + 1 == files_to_index.len(),
                    prepared.len(),
                )?);
                std::fs::remove_file(path)?;
            }
            Ok(prepared)
        })
        .await?;
        let mut splits = Vec::new();
        self.upload_splits(prepared, &split_store, &mut splits)
            .await?;
        let publication = PublishFileSplits {
            index_uid: Some(index_uid.clone()),
            file_ids: files.into_iter().map(|file| file.file_id).collect(),
            staged_split_ids: splits
                .iter()
                .map(|split| split.split_id.to_string())
                .collect(),
            publication_id: Ulid::new().to_string(),
        };
        let published = publish_files(&mut self.metastore, publication).await?;
        if published {
            // Count confirmed winners once, after publication retries have finished.
            for split in &splits {
                record_published_split(&index_uid.index_id, split);
            }
        } else {
            // These outputs belong to this attempt. Preserve them until cleanup reconciles
            // completion; a transport error alone never permits deleting any output.
            splits.clear();
        }
        Ok(Some(FilePublication {
            index_uid,
            published,
            splits,
        }))
    }

    async fn upload_splits(
        &mut self,
        prepared: Vec<PreparedSplit>,
        split_store: &IndexingSplitStore,
        uploaded: &mut Vec<SplitMetadata>,
    ) -> anyhow::Result<()> {
        for PreparedSplit {
            packaged,
            metadata,
            payload,
        } in prepared
        {
            let _permit = acquire_indexing_upload_permit(self.max_concurrent_split_uploads).await?;
            let request =
                StageSplitsRequest::try_from_split_metadata(metadata.index_uid.clone(), &metadata)?;
            self.metastore.stage_splits(request).await?;
            // Keep the same split ID and scratch files across whole-upload retries.
            // Local cache/cleanup errors can follow a successful upload and must escape.
            for attempt in 1..=3 {
                let error = match split_store
                    .store_split(
                        &metadata,
                        packaged.split_scratch_directory.path(),
                        Box::new(payload.clone()),
                    )
                    .await
                {
                    Ok(()) => break,
                    Err(error) => error,
                };
                let retry_upload = error.downcast_ref::<StorageError>().is_some_and(|error| {
                    !matches!(
                        error.kind(),
                        StorageErrorKind::Unauthorized | StorageErrorKind::NotFound
                    )
                });
                if !retry_upload || attempt == 3 {
                    return Err(error);
                }
                warn!(error = %format!("{error:#}"), split_id = %metadata.split_id(), attempt,
                    "split upload failed, retrying same split");
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
            uploaded.push(metadata);
        }
        Ok(())
    }
}

/// Runs synchronous indexing and filesystem work on the existing blocking runtime.
/// Writers, compression, and serialization can exceed the small-task CPU pool's time budget.
/// Keep the scratch root alive through cancellation: TempDirectory retains only its immediate
/// parent. The shared assignment guard also keeps detached work counted until it exits.
async fn run_indexing<T: Send + 'static>(
    directory: TempDirectory,
    active_assignment: Arc<GaugeGuard>,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    RuntimeType::Blocking
        .get_runtime_handle()
        .spawn(
            async move {
                let _directory = directory;
                let _assignment = active_assignment;
                work()
            }
            .in_current_span(),
        )
        .await?
}

async fn publish_files(
    metastore: &mut MetastoreServiceClient,
    request: PublishFileSplits,
) -> anyhow::Result<bool> {
    loop {
        match metastore.publish_file_splits(request.clone()).await {
            Ok(response) => return Ok(response.published),
            Err(error)
                if error.is_retryable() || matches!(error, MetastoreError::TooManyRequests) =>
            {
                warn!(%error, publication_id = %request.publication_id, "file publication failed, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Stops outstanding synchronous indexing work when the attempt returns or is cancelled.
struct StopIndexingOnDrop(KillSwitch);

impl Drop for StopIndexingOnDrop {
    fn drop(&mut self) {
        self.0.kill();
    }
}
