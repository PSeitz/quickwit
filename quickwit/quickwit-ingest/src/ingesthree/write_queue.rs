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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::OptionFuture;
use quickwit_cluster::Cluster;
use quickwit_common::thread_pool::run_cpu_intensive;
use quickwit_doc_mapper::DocMapper;
use quickwit_proto::ingest::{DocBatchV2, ParseFailure};
use quickwit_proto::types::{DocUid, IndexUid};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use ulid::Ulid;

use super::documents_staged_for_upload::{FRAME_OVERHEAD, MAGIC, encode_metadata};
use super::{DocumentsStagedForUpload, Ingester, OpenFileLimit, Uploader};
use crate::ingest_v2::{is_document_validation_enabled, validate_doc_batch_impl};

#[path = "capacity_tracker.rs"]
mod capacity_tracker;
use capacity_tracker::CapacityTracker;

/// Distinguishes admission failures so a router can retry another destination.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("ingesthree write queue is full")]
    QueueFull,
    #[error("ingesthree local storage budget is exhausted")]
    StorageFull,
    #[error("ingesthree open-file limit reached")]
    OpenFileLimit,
    #[error("ingesthree worker is stopped or draining")]
    Unavailable,
    #[error("invalid ingest request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug)]
pub struct AppendResponse {
    /// `None` when all documents fail validation and no file is created.
    pub file_id: Option<Ulid>,
    pub num_docs: u64,
    pub parse_failures: Vec<ParseFailure>,
}

/// Queues document batches for validation and writing to per-index files.
/// Clones share queue and disk byte limits.
///
/// Queue bytes cover queued and processing input, including a validation copy and
/// descriptors. Validator scratch and response allocations are separate. Disk
/// reservations allow a new file header per request and settle to file growth
/// after writes. Filesystem overhead and reserves are separate.
#[derive(Clone)]
pub struct WriteQueue {
    sender: mpsc::UnboundedSender<WriterRequest>,
    admission: Arc<Mutex<Admission>>,
}

// Tracks byte limits, usage, reservations, and whether admission is closed.
struct Admission {
    max_queue_bytes: u64,
    queue_bytes: u64,
    max_disk_bytes: u64,
    disk_bytes: u64,
    reserved_disk_bytes: u64,
    closed: bool,
}

impl Admission {
    fn capacity_score(&self, tracker: &mut CapacityTracker) -> usize {
        if self.closed || self.max_disk_bytes == 0 {
            return 0;
        }
        tracker.record_and_score(self.disk_bytes + self.reserved_disk_bytes, self.queue_bytes)
    }
}

// Holds queue and disk capacity until request buffers are released.
// The writer retains the reservation when the caller drops the reply receiver.
struct Reservation {
    admission: Arc<Mutex<Admission>>,
    queue_bytes: u64,
    disk_bytes: u64,
}

impl Reservation {
    /// Releases the reservation after a write, adding written bytes to disk usage.
    fn release_after_write(mut self, written_bytes: u64) {
        self.release(written_bytes);
    }

    fn release(&mut self, written_bytes: u64) {
        assert!(written_bytes <= self.disk_bytes);
        let mut admission = self.admission.lock().expect("admission mutex poisoned");
        // Transfer reservations to retained disk usage atomically with admission.
        admission.disk_bytes += written_bytes;
        admission.queue_bytes -= self.queue_bytes;
        admission.reserved_disk_bytes -= self.disk_bytes;
        self.queue_bytes = 0;
        self.disk_bytes = 0;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.queue_bytes != 0 {
            self.release(0);
        }
    }
}

// A document batch to append, with its reply sender and byte reservation.
struct AppendRequest {
    index_uid: IndexUid,
    batch: DocBatchV2,
    mapper: Option<Arc<DocMapper>>,
    reply: oneshot::Sender<Result<AppendResponse, IngestError>>,
    // Last so queued request buffers drop before releasing their reservation.
    reservation: Reservation,
}

// Requests that the writer append a batch, finish locally, or drain handoff.
enum WriterRequest {
    Append(AppendRequest),
    Finish(oneshot::Sender<Result<Vec<DocumentsStagedForUpload>, IngestError>>),
    Drain(oneshot::Sender<Result<(), IngestError>>),
}

impl WriteQueue {
    /// Recovers local files and completes upload, registration, and local deletion
    /// before opening intake when an uploader is configured. Cancelling startup
    /// retains remaining files and releases ownership after local filesystem work ends.
    pub async fn open(
        data_dir: PathBuf,
        max_queue_bytes: u64,
        max_disk_bytes: u64,
        uploader: Option<Uploader>,
    ) -> Result<Self, IngestError> {
        if max_queue_bytes == 0 {
            return Err(IngestError::InvalidRequest(
                "zero write-queue budget".into(),
            ));
        }
        let mut ingester =
            tokio::task::spawn_blocking(move || Ingester::open(&data_dir, max_disk_bytes))
                .await
                .map_err(anyhow::Error::from)??;
        if let Some(uploader) = &uploader {
            while let Some(input) = ingester.finished_files().first() {
                let file_id = uploader.start(input).await?;
                // Once registered or discarded, retry only local deletion. No intake exists
                // yet, so the ingester's disk accounting is the only charge to release.
                loop {
                    let (writer, result) = tokio::task::spawn_blocking(move || {
                        let result = ingester.delete_finished_file(file_id);
                        (ingester, result)
                    })
                    .await
                    .map_err(anyhow::Error::from)?;
                    ingester = writer;
                    match result {
                        Ok(_) => break,
                        Err(error) => {
                            tracing::warn!(%file_id, ?error, "failed to delete finished ingesthree file; retrying");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        }
        let (queue, receiver) = Self::channel(&ingester, max_queue_bytes);
        let admission = queue.admission.clone();
        tokio::spawn(async move {
            if let Err(error) = write_batches(ingester, receiver, admission, uploader).await {
                tracing::error!(error = ?error, "ingesthree writer stopped");
            }
        });
        Ok(queue)
    }

    /// Creates a channel whose sender reserves queue and disk bytes.
    fn channel(
        ingester: &Ingester,
        max_queue_bytes: u64,
    ) -> (Self, mpsc::UnboundedReceiver<WriterRequest>) {
        // Each append reserves bytes for its request structure, limiting the message count.
        let (sender, receiver) = mpsc::unbounded_channel();
        let admission = Arc::new(Mutex::new(Admission {
            max_queue_bytes,
            queue_bytes: 0,
            max_disk_bytes: ingester.max_disk_bytes,
            disk_bytes: ingester.disk_bytes,
            reserved_disk_bytes: 0,
            closed: false,
        }));
        (Self { sender, admission }, receiver)
    }

    /// Queues a batch for validation and writing, then waits for the append result.
    /// `Some(mapper)` enables JSON/schema validation unless
    /// `QW_DISABLE_DOCUMENT_VALIDATION` is true. `None` skips validation.
    /// The caller resolves the index and its configuration before calling this API.
    pub async fn append(
        &self,
        index_uid: IndexUid,
        batch: DocBatchV2,
        mapper: Option<Arc<DocMapper>>,
    ) -> Result<AppendResponse, IngestError> {
        let (queue_bytes, disk_bytes) = num_bytes(&index_uid, &batch)?;
        let (reply, response) = oneshot::channel();
        {
            let mut admission = self.admission.lock().expect("admission mutex poisoned");
            if admission.closed || self.sender.is_closed() {
                return Err(IngestError::Unavailable);
            }
            let total_queue_bytes = admission
                .queue_bytes
                .checked_add(queue_bytes)
                .filter(|total| *total <= admission.max_queue_bytes)
                .ok_or(IngestError::QueueFull)?;
            let total_disk_bytes = admission
                .disk_bytes
                .checked_add(admission.reserved_disk_bytes)
                .and_then(|total| total.checked_add(disk_bytes))
                .filter(|total| *total <= admission.max_disk_bytes);
            if total_disk_bytes.is_none() {
                return Err(IngestError::StorageFull);
            }
            admission.queue_bytes = total_queue_bytes;
            admission.reserved_disk_bytes += disk_bytes;
            let request = WriterRequest::Append(AppendRequest {
                index_uid,
                batch,
                mapper,
                reply,
                reservation: Reservation {
                    admission: self.admission.clone(),
                    queue_bytes,
                    disk_bytes,
                },
            });
            // Sending under the lock orders admission against finish. Drop a
            // failed send only after unlocking, since its reservation also locks.
            let sent = self.sender.send(request);
            drop(admission);
            sent.map_err(|_| IngestError::Unavailable)?;
        }
        response.await.map_err(|_| IngestError::Unavailable)?
    }

    /// Waits until the writer closes its request channel, during shutdown or after a failure.
    pub async fn wait_for_closed(&self) {
        self.sender.closed().await;
    }

    /// Publishes node-wide capacity until the writer closes. Uses the v2 score and
    /// its five-second sampling cadence. Feedback is advisory: append still checks
    /// reservations atomically, including work admitted since the last sample.
    /// The caller must poll this future throughout intake and cancel it on shutdown.
    pub async fn broadcast_capacity(&self, cluster: &Cluster) {
        let mut tracker = {
            let admission = self.admission.lock().expect("admission mutex poisoned");
            CapacityTracker::new(admission.max_disk_bytes.max(1), admission.max_queue_bytes)
        };
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                biased;
                _ = self.wait_for_closed() => break,
                _ = interval.tick() => {
                    let score = self.admission.lock().expect("admission mutex poisoned")
                        .capacity_score(&mut tracker);
                    cluster.set_self_ingesthree_capacity(score).await;
                }
            }
        }
        cluster.set_self_ingesthree_capacity(0).await;
    }

    /// Stops admission across all clones, cancels pending handoff, drains queued
    /// writes, and finishes files locally. Returns files still retained on disk.
    /// Use `drain()` for decommissioning.
    pub async fn finish(&self) -> Result<Vec<DocumentsStagedForUpload>, IngestError> {
        let (reply, response) = oneshot::channel();
        {
            let mut admission = self.admission.lock().expect("admission mutex poisoned");
            if admission.closed {
                return Err(IngestError::Unavailable);
            }
            admission.closed = true;
            self.sender
                .send(WriterRequest::Finish(reply))
                .map_err(|_| IngestError::Unavailable)?;
        }
        response.await.map_err(|_| IngestError::Unavailable)?
    }

    /// Stops admission across all clones, finishes queued writes, then waits for
    /// upload, registration, and local deletion. Requires a configured uploader.
    /// Success releases the directory and all disk charges. Transient failures retry.
    /// Permanent errors retain local data and stop the writer. Dropping this future
    /// cancels handoff and releases ownership after local filesystem work completes.
    /// Admission remains closed on error or cancellation. Reopen to resume recovery.
    pub async fn drain(&self) -> Result<(), IngestError> {
        let (reply, response) = oneshot::channel();
        {
            let mut admission = self.admission.lock().expect("admission mutex poisoned");
            if admission.closed {
                return Err(IngestError::Unavailable);
            }
            admission.closed = true;
            self.sender
                .send(WriterRequest::Drain(reply))
                .map_err(|_| IngestError::Unavailable)?;
        }
        response.await.map_err(|_| IngestError::Unavailable)?
    }
}

/// Calculates queue and disk byte counts, allowing a new file header.
fn num_bytes(index_uid: &IndexUid, batch: &DocBatchV2) -> Result<(u64, u64), IngestError> {
    if batch.doc_lengths.is_empty() || batch.doc_lengths.len() != batch.doc_uids.len() {
        return Err(IngestError::InvalidRequest(
            "empty or inconsistent document batch".into(),
        ));
    }
    let metadata = encode_metadata(index_uid)
        .map_err(|error| IngestError::InvalidRequest(error.to_string()))?;
    // Count input buffers, a possible filtered payload, and request overhead.
    let mut queue_bytes = size_of::<AppendRequest>() as u64 + metadata.len() as u64;
    for (count, size) in [
        (batch.doc_buffer.len(), 2),
        (batch.doc_lengths.capacity(), size_of::<u32>()),
        (batch.doc_uids.capacity(), size_of::<DocUid>()),
        (batch.doc_lengths.len(), size_of::<Bytes>()),
    ] {
        queue_bytes = (count as u64)
            .checked_mul(size as u64)
            .and_then(|bytes| queue_bytes.checked_add(bytes))
            .ok_or_else(|| IngestError::InvalidRequest("request size overflow".into()))?;
    }
    let disk_bytes = (batch.doc_lengths.len() as u64)
        .checked_mul(FRAME_OVERHEAD)
        .and_then(|total| total.checked_add(batch.doc_buffer.len() as u64))
        .and_then(|total| {
            total.checked_add(MAGIC.len() as u64 + FRAME_OVERHEAD + metadata.len() as u64)
        })
        .ok_or_else(|| IngestError::InvalidRequest("input size overflow".into()))?;
    Ok((queue_bytes, disk_bytes))
}

/// Validates a batch and returns its documents and parse failures.
fn validate_documents(
    batch: DocBatchV2,
    mapper: Option<Arc<DocMapper>>,
) -> Result<(Vec<Bytes>, Vec<ParseFailure>), IngestError> {
    let document_bytes = batch
        .doc_lengths
        .iter()
        .try_fold(0usize, |sum, len| sum.checked_add(*len as usize));
    if document_bytes != Some(batch.doc_buffer.len())
        || batch.doc_uids.iter().collect::<HashSet<_>>().len() != batch.doc_uids.len()
    {
        return Err(IngestError::InvalidRequest(
            "inconsistent lengths or duplicate document IDs".into(),
        ));
    }
    let (batch, parse_failures) = match mapper {
        Some(mapper) if is_document_validation_enabled() => validate_doc_batch_impl(batch, &mapper),
        _ => (batch, Vec::new()),
    };
    Ok((
        batch.docs().map(|(_, bytes)| bytes).collect(),
        parse_failures,
    ))
}

/// Writes batches and finishes files while upload and registration make progress.
async fn write_batches(
    ingester: Ingester,
    receiver: mpsc::UnboundedReceiver<WriterRequest>,
    admission: Arc<Mutex<Admission>>,
    uploader: Option<Uploader>,
) -> anyhow::Result<()> {
    let mut drain_reply = None;
    let result =
        write_until_stopped(ingester, receiver, &admission, uploader, &mut drain_reply).await;
    admission.lock().expect("admission mutex poisoned").closed = true;
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
    mut ingester: Ingester,
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
                        admission.lock().expect("admission mutex poisoned").closed = true;
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
                    Ok(num_bytes) => {
                        admission.lock().expect("admission mutex poisoned").disk_bytes -= num_bytes;
                        pending_delete = None;
                    }
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

/// Validates and writes one batch, settles its reservation, and replies to the caller.
async fn append_batch(mut ingester: Ingester, append: AppendRequest) -> anyhow::Result<Ingester> {
    let AppendRequest {
        index_uid,
        batch,
        mapper,
        reply,
        reservation,
    } = append;
    let (documents, parse_failures) =
        match run_cpu_intensive(move || validate_documents(batch, mapper)).await? {
            Ok(documents_and_failures) => documents_and_failures,
            Err(error) => {
                drop(reservation);
                let _ = reply.send(Err(error));
                return Ok(ingester);
            }
        };
    let num_docs = documents.len() as u64;
    if documents.is_empty() {
        drop(documents);
        drop(reservation);
        let _ = reply.send(Ok(AppendResponse {
            file_id: None,
            num_docs,
            parse_failures,
        }));
        return Ok(ingester);
    }
    let (writer, result, written_bytes) = tokio::task::spawn_blocking(move || {
        let previous_disk_bytes = ingester.disk_bytes;
        let result = ingester.append(&index_uid, &documents);
        let written_bytes = ingester.disk_bytes - previous_disk_bytes;
        // Release document buffers before releasing their reservation.
        drop(documents);
        (ingester, result, written_bytes)
    })
    .await?;
    ingester = writer;
    match result {
        Ok(file_id) => {
            reservation.release_after_write(written_bytes);
            let _ = reply.send(Ok(AppendResponse {
                file_id: Some(file_id),
                num_docs,
                parse_failures,
            }));
        }
        Err(error) if error.is::<OpenFileLimit>() => {
            drop(reservation);
            let _ = reply.send(Err(IngestError::OpenFileLimit));
        }
        Err(error) => {
            let message = format!("{error:#}");
            reservation
                .admission
                .lock()
                .expect("admission mutex poisoned")
                .closed = true;
            // A failed BufWriter may still hold bytes. Its destructor also
            // belongs on the blocking pool, before releasing the directory.
            tokio::task::spawn_blocking(move || drop(ingester)).await?;
            let _ = reply.send(Err(IngestError::Internal(error)));
            anyhow::bail!("local append failed: {message}");
        }
    }
    Ok(ingester)
}

#[cfg(test)]
#[path = "write_queue_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "file_limits_tests.rs"]
mod file_limits_tests;

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod handoff_tests;
