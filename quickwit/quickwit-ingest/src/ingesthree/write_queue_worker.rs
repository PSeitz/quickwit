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

//! Validates and compresses queued batches, then drives local writes and upload handoff.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::OptionFuture;
use quickwit_common::thread_pool::run_cpu_intensive;
use quickwit_doc_mapper::DocMapper;
use quickwit_proto::ingest::{DocBatchV2, ParseFailure};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use ulid::Ulid;

use super::{
    Admission, AppendRequest, AppendResponse, CompressedBatch, FileWriter, IngestError,
    PreparedRequest, ValidationMetrics, WriterRequest,
};
use crate::ingest_v2::{is_document_validation_enabled, validate_doc_batch_impl};
use crate::ingesthree::{OpenFileLimit, Uploader};

impl AppendRequest {
    pub(super) fn prepare(self, metrics: &ValidationMetrics) -> PreparedRequest {
        let Self {
            index_uid,
            batch,
            mapper,
            reply,
            reservation,
        } = self;
        // Validation and compression own their buffers inside this call, so they
        // drop before the reservation on errors and panics as well as success.
        let preparation =
            validate_documents(batch, mapper, metrics).and_then(|(batch, failures)| {
                let compressed = if batch.is_empty() {
                    None
                } else {
                    Some(CompressedBatch::new(
                        batch.docs().map(|(_, document)| document),
                    )?)
                };
                Ok((compressed, failures))
            });
        PreparedRequest {
            index_uid,
            reply,
            preparation,
            reservation,
        }
    }
}

/// Validates a batch and returns its valid documents and parse failures.
pub(super) fn validate_documents(
    batch: DocBatchV2,
    mapper: Option<Arc<DocMapper>>,
    metrics: &ValidationMetrics,
) -> Result<(DocBatchV2, Vec<ParseFailure>), IngestError> {
    let document_bytes = batch
        .doc_lengths
        .iter()
        .try_fold(0usize, |sum, len| sum.checked_add(*len as usize));
    if document_bytes != Some(batch.doc_buffer.len()) {
        return Err(IngestError::InvalidRequest(
            "inconsistent document lengths".into(),
        ));
    }
    let original_bytes = batch.num_bytes();
    let (batch, parse_failures) = match mapper {
        Some(mapper) if is_document_validation_enabled() => validate_doc_batch_impl(batch, &mapper),
        _ => (batch, Vec::new()),
    };
    metrics.record(&batch, parse_failures.len(), original_bytes);
    Ok((batch, parse_failures))
}

/// Writes batches and finishes files while upload and registration make progress.
pub(super) async fn write_batches(
    ingester: FileWriter,
    receiver: mpsc::UnboundedReceiver<WriterRequest>,
    admission: Arc<Mutex<Admission>>,
    uploader: Option<Uploader>,
) -> anyhow::Result<()> {
    let mut drain_reply = None;
    let result =
        write_until_stopped(ingester, receiver, &admission, uploader, &mut drain_reply).await;
    admission.lock().expect("admission mutex poisoned").close();
    // The owner and any upload future are dropped before acknowledging drain.
    if let Some(reply) = drain_reply {
        let response = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| IngestError::Internal(anyhow::anyhow!("{error:#}")));
        let _ = reply.send(response);
    }
    result
}

async fn write_until_stopped(
    mut ingester: FileWriter,
    mut receiver: mpsc::UnboundedReceiver<WriterRequest>,
    admission: &Arc<Mutex<Admission>>,
    uploader: Option<Uploader>,
    drain_reply: &mut Option<oneshot::Sender<Result<(), IngestError>>>,
) -> anyhow::Result<()> {
    // Local IO errors or panics can drop the ingester on a blocking thread. Keep
    // directory ownership until handoff is dropped too (locals drop in reverse order).
    let directory_lock = ingester.directory_lock.clone();
    // Handoff completes after registration or remote discard. Then delete the local file.
    let mut handoff = None;
    let mut pending_delete: Option<(Ulid, Instant)> = None;
    loop {
        if drain_reply.is_some() && ingester.finished.is_empty() {
            break;
        }
        if handoff.is_none()
            && pending_delete.is_none()
            && let (Some(uploader), Some(input)) = (&uploader, ingester.finished.first())
        {
            handoff = Some(uploader.start(input));
        }
        tokio::select! {
            // Cancellation and expiry take priority even when the request queue stays nonempty.
            biased;
            _ = async {
                match drain_reply.as_mut() {
                    Some(reply) => reply.closed().await,
                    None => std::future::pending().await,
                }
            } => break,
            _ = wait_for_deadline(ingester.next_deadline()) => {
                let (writer, result) = tokio::task::spawn_blocking(move || {
                    let result = ingester.finish_expired_files();
                    (ingester, result)
                }).await?;
                ingester = writer;
                if let Err(error) = result {
                    receiver.close();
                    drop(handoff);
                    tokio::task::spawn_blocking(move || drop(ingester)).await?;
                    return Err(error.context("failed to finish local files"));
                }
            }
            Some(result) = OptionFuture::from(handoff.as_mut()) => {
                handoff = None;
                match result {
                    Ok(file_id) => pending_delete = Some((file_id, Instant::now())),
                    Err(error) => {
                        receiver.close();
                        admission.lock().expect("admission mutex poisoned").close();
                        // Preserve accepted writes and release the directory on the blocking pool.
                        tokio::task::spawn_blocking(move || ingester.finish()).await??;
                        return Err(error);
                    }
                }
            }
            Some(file_id) = async {
                let (file_id, retry_at) = pending_delete?;
                if retry_at > Instant::now() {
                    tokio::time::sleep_until(retry_at).await;
                }
                Some(file_id)
            } => {
                let (writer, result) = tokio::task::spawn_blocking(move || {
                    let result = ingester.delete_finished_file(file_id);
                    (ingester, result)
                }).await?;
                ingester = writer;
                match result {
                    Ok(_) => pending_delete = None,
                    Err(error) => {
                        tracing::warn!(%file_id, ?error, "failed to delete finished ingesthree file; retrying");
                        pending_delete = Some((file_id, Instant::now() + Duration::from_secs(1)));
                    }
                }
            }
            request = receiver.recv(), if drain_reply.is_none() => match request {
                Some(WriterRequest::Append(append)) => {
                    ingester = append_batch(ingester, append).await?;
                }
                Some(WriterRequest::Finish(reply)) => {
                    drop(handoff);
                    let result = tokio::task::spawn_blocking(move || ingester.finish()).await?;
                    drop(directory_lock);
                    let _ = reply.send(result.map_err(IngestError::Internal));
                    return Ok(());
                }
                Some(WriterRequest::Drain(reply)) => {
                    *drain_reply = Some(reply);
                    receiver.close();
                    let (writer, result) = tokio::task::spawn_blocking(move || {
                        let result = ingester.finish_files();
                        (ingester, result)
                    }).await?;
                    ingester = writer;
                    if let Err(error) = result {
                        drop(handoff);
                        tokio::task::spawn_blocking(move || drop(ingester)).await?;
                        return Err(error.context("failed to finish local files"));
                    }
                    if uploader.is_none() {
                        tokio::task::spawn_blocking(move || drop(ingester)).await?;
                        anyhow::bail!("handoff drain requires an uploader");
                    }
                }
                None => break,
            },
        }
    }
    drop(handoff);
    tokio::task::spawn_blocking(move || ingester.finish()).await??;
    Ok(())
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Prepares and writes one batch, settles its reservation, and replies to the caller.
pub(super) async fn append_batch(
    ingester: FileWriter,
    append: AppendRequest,
) -> anyhow::Result<FileWriter> {
    let metrics = append
        .reservation
        .admission
        .lock()
        .expect("admission mutex poisoned")
        .metrics
        .validation
        .clone();
    // Move the request into preparation. Cancellation can leave CPU work running,
    // and a discarded result must drop its buffers before its reservation.
    let prepared = run_cpu_intensive(move || append.prepare(&metrics)).await?;
    tokio::task::spawn_blocking(move || append_prepared(ingester, prepared)).await?
}

pub(super) fn append_prepared(
    mut ingester: FileWriter,
    prepared: PreparedRequest,
) -> anyhow::Result<FileWriter> {
    let PreparedRequest {
        index_uid,
        reply,
        preparation,
        reservation,
    } = prepared;
    // Declared after the reservation so the batch drops first during unwinding.
    let (batch, parse_failures) = match preparation {
        Ok(documents_and_failures) => documents_and_failures,
        Err(error) => {
            drop(reservation);
            let _ = reply.send(Err(error));
            return Ok(ingester);
        }
    };
    let previous_disk_bytes = ingester.disk_bytes;
    let num_docs = batch.as_ref().map_or(0, |batch| batch.num_docs);
    let result = match &batch {
        Some(batch) => ingester.append_compressed(&index_uid, batch).map(Some),
        None => Ok(None),
    };
    let written_bytes = ingester.disk_bytes - previous_disk_bytes;
    drop(batch);
    match result {
        Ok(file_id) => {
            // Settle in the blocking task even if its awaiting future was cancelled.
            reservation.release_after_write(written_bytes);
            let _ = reply.send(Ok(AppendResponse {
                file_id,
                num_docs,
                parse_failures,
            }));
        }
        Err(error) if error.is::<OpenFileLimit>() => {
            let error = reservation
                .admission
                .lock()
                .expect("admission mutex poisoned")
                .metrics
                .reject(IngestError::OpenFileLimit);
            drop(reservation);
            let _ = reply.send(Err(error));
        }
        Err(error) => {
            let message = format!("{error:#}");
            {
                let mut admission = reservation
                    .admission
                    .lock()
                    .expect("admission mutex poisoned");
                admission.close();
                // A rename failure can follow a flushed append. Retain its known
                // file growth. Recovery reconciles partial writes after IO errors.
                admission.record_written(written_bytes);
            }
            // A failed BufWriter may still hold bytes. Drop it on this blocking
            // thread before releasing its reservation and directory ownership.
            drop(ingester);
            let _ = reply.send(Err(IngestError::Internal(error)));
            anyhow::bail!("local append failed: {message}");
        }
    }
    Ok(ingester)
}
