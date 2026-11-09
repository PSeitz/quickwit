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
mod supervisor;
pub(crate) use supervisor::S3IndexerSupervisor;
pub use supervisor::StopS3Indexers;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use bytes::Bytes;
use quickwit_common::KillSwitch;
use quickwit_common::io::{IoControls, Limiter};
use quickwit_common::retry::Retryable;
use quickwit_common::runtimes::RuntimeType;
use quickwit_common::temp_dir::TempDirectory;
use quickwit_common::thread_pool::run_cpu_intensive;
use quickwit_common::uri::Uri;
use quickwit_config::RetentionPolicy;
use quickwit_ingest::ingesthree::DocumentFileReader;
use quickwit_metastore::{IndexMetadataResponseExt, SplitMetadata, StageSplitsRequestExt};
use quickwit_proto::indexing::IndexingPipelineId;
use quickwit_proto::metastore::{
    AssignFilesForIndexing, IndexMetadataRequest, LastDeleteOpstampRequest, MetastoreError,
    MetastoreService, MetastoreServiceClient, PublishFileSplits, StageSplitsRequest,
};
use quickwit_proto::types::{IndexUid, NodeId, PipelineUid, SourceId};
use quickwit_storage::StorageResolver;
use tracing::{Instrument, warn};
use ulid::Ulid;

use self::indexing::SplitBuilders;
use crate::actors::{
    acquire_indexing_upload_permit, create_packaged_split, prepare_split_for_upload,
};
use crate::docs_clustering::Fingerprinter;
use crate::merge_policy::{MergePolicy, merge_policy_from_settings};
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
        let directory = self.scratch_directory.clone();
        let directory =
            tokio::task::spawn_blocking(move || directory.named_temp_child("files-")).await??;
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
        let split_directory = directory.clone();
        let fingerprinter = self.fingerprinter.clone();
        let mut builders = run_cpu_intensive(move || {
            SplitBuilders::new(
                config,
                pipeline_id,
                delete_opstamp,
                split_directory,
                io_controls,
                fingerprinter,
            )
        })
        .await??;
        let mut splits = Vec::new();
        for file in &files {
            let uri: Uri = file.file_uri.parse()?;
            let storage = self
                .storage_resolver
                .resolve(&uri.parent().context("input URI has no parent")?)
                .await?;
            // Use a fixed local filename. The directory belongs to this attempt and is reused
            // only after the previous input has been read and removed.
            let path = directory.path().join("input");
            let size = storage
                .copy_to_file(uri.file_name().context("input URI has no filename")?, &path)
                .await?;
            ensure!(
                size == file.num_bytes,
                "downloaded input size differs from registration"
            );
            let expected_uid = index_uid.clone();
            let expected_size = file.num_bytes;
            let mut reader = run_indexing(directory.clone(), move || {
                DocumentFileReader::open(&path, &expected_uid, expected_size)
            })
            .await?;
            loop {
                let (next_reader, documents) =
                    run_indexing(directory.clone(), move || read_batch(reader)).await?;
                reader = next_reader;
                if documents.is_empty() {
                    break;
                }
                builders = run_indexing(directory.clone(), move || {
                    builders.add_documents(documents)?;
                    Ok(builders)
                })
                .await?;
                if builders.should_flush() {
                    self.upload_splits(
                        &mut builders,
                        &split_store,
                        &merge_policy,
                        retention.as_ref(),
                        &mut splits,
                    )
                    .await?;
                }
            }
            drop(reader);
            tokio::fs::remove_file(directory.path().join("input")).await?;
        }
        self.upload_splits(
            &mut builders,
            &split_store,
            &merge_policy,
            retention.as_ref(),
            &mut splits,
        )
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
        if !published {
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
        builders: &mut SplitBuilders,
        split_store: &IndexingSplitStore,
        merge_policy: &Arc<dyn MergePolicy>,
        retention: Option<&RetentionPolicy>,
        uploaded: &mut Vec<SplitMetadata>,
    ) -> anyhow::Result<()> {
        let splits = builders.take_splits();
        ensure!(
            uploaded.len() + splits.len() <= 1000,
            "assigned group exceeds the publication split limit"
        );
        for split in splits {
            let _permit = acquire_indexing_upload_permit(self.max_concurrent_split_uploads).await?;
            let io_controls = builders
                .io_controls
                .clone()
                .set_component("index_serializer");
            let tags = builders.tag_fields()?;
            let merge_policy = merge_policy.clone();
            let retention = retention.cloned();
            let (packaged, metadata, payload) =
                run_indexing(self.scratch_directory.clone(), move || {
                    let _guard = io_controls.check_if_alive()?;
                    split
                        .controlled_directory
                        .set_io_controls(io_controls.clone());
                    let packaged =
                        create_packaged_split(split.finalize()?, &tags, io_controls.progress())?;
                    let _guard = io_controls.check_if_alive()?;
                    let (metadata, payload) =
                        prepare_split_for_upload(&packaged, &merge_policy, retention.as_ref())?;
                    Ok((packaged, metadata, payload))
                })
                .await?;
            let request =
                StageSplitsRequest::try_from_split_metadata(metadata.index_uid.clone(), &metadata)?;
            self.metastore.stage_splits(request).await?;
            split_store
                .store_split(
                    &metadata,
                    packaged.split_scratch_directory.path(),
                    Box::new(payload),
                )
                .await?;
            uploaded.push(metadata);
        }
        Ok(())
    }
}

/// Runs synchronous indexing and filesystem work on the existing blocking runtime.
/// Writers, compression, and serialization can exceed the small-task CPU pool's time budget.
/// Keep the scratch root alive through cancellation: TempDirectory retains only its immediate
/// parent.
async fn run_indexing<T: Send + 'static>(
    directory: TempDirectory,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    RuntimeType::Blocking
        .get_runtime_handle()
        .spawn(
            async move {
                let _directory = directory;
                work()
            }
            .in_current_span(),
        )
        .await?
}

/// Read a small batch. The final document can exceed the byte target.
fn read_batch(mut reader: DocumentFileReader) -> anyhow::Result<(DocumentFileReader, Vec<Bytes>)> {
    let mut documents = Vec::new();
    let mut num_bytes = 0;
    while documents.len() < 128 && num_bytes < 1024 * 1024 {
        let Some(document) = reader.read_document()? else {
            break;
        };
        num_bytes += document.len();
        documents.push(document);
    }
    Ok((reader, documents))
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
