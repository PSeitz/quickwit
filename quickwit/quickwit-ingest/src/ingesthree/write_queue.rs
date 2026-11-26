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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(test)]
use bytes::Bytes;
use quickwit_cluster::Cluster;
use quickwit_doc_mapper::DocMapper;
use quickwit_proto::ingest::{DocBatchV2, ParseFailure};
use quickwit_proto::types::{DocUid, IndexUid};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use super::documents_staged_for_upload::{FRAME_OVERHEAD, MAGIC, encode_metadata};
use super::handoff::FileHandoffs;
use super::{CompressedBatch, DocumentsStagedForUpload, Ingester, Uploader};

#[path = "capacity_tracker.rs"]
mod capacity_tracker;
use capacity_tracker::CapacityTracker;

#[path = "write_queue_worker.rs"]
mod worker;
use worker::write_batches;
#[cfg(test)]
use worker::{append_batch, append_prepared, validate_documents};

#[path = "queue_metrics.rs"]
mod queue_metrics;
use queue_metrics::{QueueMetrics, ValidationMetrics};

#[path = "queue_file_writer.rs"]
mod queue_file_writer;
use queue_file_writer::FileWriter;

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
/// Queue bytes cover input, validation copies/descriptors, compression serialization
/// (document bytes + four bytes per document), and maximum compressed buffer capacity.
/// Validator and zstd working scratch and response allocations are separate. Disk
/// reservations cover the maximum compressed record plus a new file header per request
/// and settle to actual file growth. Filesystem overhead and reserves are separate.
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
    metrics: QueueMetrics,
}

impl Admission {
    fn capacity_score(&mut self, tracker: &mut CapacityTracker) -> usize {
        let score = if self.closed || self.max_disk_bytes == 0 {
            0
        } else {
            tracker.record_and_score(self.disk_bytes + self.reserved_disk_bytes, self.queue_bytes)
        };
        self.metrics.record_capacity(score);
        score
    }

    fn record_usage(&self) {
        self.metrics
            .record_usage(self.queue_bytes, self.reserved_disk_bytes);
    }

    fn close(&mut self) {
        self.closed = true;
        self.metrics.record_capacity(0);
    }

    fn record_written(&mut self, written_bytes: u64) {
        self.disk_bytes += written_bytes;
        // Releasing old validation work after reopen must leave the recovered
        // disk snapshot untouched. Only file mutations update that snapshot.
        if written_bytes > 0 {
            self.metrics.disk_bytes.set(self.disk_bytes as f64);
        }
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
        admission.record_written(written_bytes);
        admission.queue_bytes -= self.queue_bytes;
        admission.reserved_disk_bytes -= self.disk_bytes;
        admission.record_usage();
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

// A prepared result whose compressed buffer drops before its input reservation.
struct PreparedRequest {
    index_uid: IndexUid,
    reply: oneshot::Sender<Result<AppendResponse, IngestError>>,
    preparation: Result<(Option<CompressedBatch>, Vec<ParseFailure>), IngestError>,
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
        let (queue, receiver, mut ingester) = tokio::task::spawn_blocking(move || {
            let ingester = Ingester::open(&data_dir, max_disk_bytes)?;
            let (queue, receiver) = Self::channel(&ingester, max_queue_bytes);
            let ingester = FileWriter::new(ingester, queue.admission.clone());
            Ok::<_, anyhow::Error>((queue, receiver, ingester))
        })
        .await
        .map_err(anyhow::Error::from)??;
        if let Some(uploader) = &uploader {
            // Keep ownership until every handoff is dropped, including when local
            // deletion fails or startup is cancelled during blocking filesystem work.
            let _directory_lock = ingester.directory_lock.clone();
            let mut handoffs = FileHandoffs::default();
            handoffs.schedule_uploads(uploader, ingester.finished_files());
            while !ingester.finished_files().is_empty() {
                let file_id = handoffs.next_delete().await?;
                let (writer, result) = tokio::task::spawn_blocking(move || {
                    let result = ingester.delete_finished_file(file_id);
                    (ingester, result)
                })
                .await
                .map_err(anyhow::Error::from)?;
                ingester = writer;
                match result {
                    Ok(_) => handoffs.deleted(file_id),
                    Err(error) => {
                        tracing::warn!(%file_id, ?error, "failed to delete finished ingesthree file; retrying");
                        handoffs.retry_delete(file_id);
                    }
                }
            }
        }
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
            metrics: QueueMetrics::new(
                max_queue_bytes,
                ingester.max_disk_bytes,
                ingester.disk_bytes,
            ),
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
                return Err(admission.metrics.reject(IngestError::Unavailable));
            }
            let total_queue_bytes = admission
                .queue_bytes
                .checked_add(queue_bytes)
                .filter(|total| *total <= admission.max_queue_bytes)
                .ok_or_else(|| admission.metrics.reject(IngestError::QueueFull))?;
            let total_disk_bytes = admission
                .disk_bytes
                .checked_add(admission.reserved_disk_bytes)
                .and_then(|total| total.checked_add(disk_bytes))
                .filter(|total| *total <= admission.max_disk_bytes);
            if total_disk_bytes.is_none() {
                return Err(admission.metrics.reject(IngestError::StorageFull));
            }
            admission.queue_bytes = total_queue_bytes;
            admission.reserved_disk_bytes += disk_bytes;
            admission.record_usage();
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
            if let Err(error) = sent {
                drop(error);
                let admission = self.admission.lock().expect("admission mutex poisoned");
                return Err(admission.metrics.reject(IngestError::Unavailable));
            }
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
        self.admission
            .lock()
            .expect("admission mutex poisoned")
            .metrics
            .record_capacity(0);
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
            admission.close();
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
            admission.close();
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
    let compressed_capacity =
        CompressedBatch::max_encoded_len(batch.doc_buffer.len(), batch.doc_lengths.len())
            .map_err(|error| IngestError::InvalidRequest(error.to_string()))?;
    // Input, validation copy/descriptors, serialization, and the full compressed
    // allocation coexist during preparation. zstd's working context is separate.
    let mut queue_bytes = size_of::<AppendRequest>() as u64 + metadata.len() as u64;
    for (count, size) in [
        (batch.doc_buffer.len(), 3),
        (batch.doc_lengths.len(), size_of::<u32>()),
        (compressed_capacity, 1),
        (batch.doc_lengths.capacity(), size_of::<u32>()),
        (batch.doc_uids.capacity(), size_of::<DocUid>()),
        // Filtering grows descriptor vectors geometrically while retaining the input.
        (
            batch.doc_lengths.len(),
            2 * (size_of::<u32>() + size_of::<DocUid>()),
        ),
    ] {
        queue_bytes = (count as u64)
            .checked_mul(size as u64)
            .and_then(|bytes| queue_bytes.checked_add(bytes))
            .ok_or_else(|| IngestError::InvalidRequest("request size overflow".into()))?;
    }
    let disk_bytes = (compressed_capacity as u64)
        .checked_add(FRAME_OVERHEAD)
        .and_then(|total| {
            total.checked_add(MAGIC.len() as u64 + FRAME_OVERHEAD + metadata.len() as u64)
        })
        .ok_or_else(|| IngestError::InvalidRequest("input size overflow".into()))?;
    Ok((queue_bytes, disk_bytes))
}

#[cfg(test)]
#[path = "write_queue_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "queue_metrics_tests.rs"]
mod queue_metrics_tests;

#[cfg(test)]
#[path = "validation_metrics_tests.rs"]
mod validation_metrics_tests;

#[cfg(test)]
#[path = "file_limits_tests.rs"]
mod file_limits_tests;

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod handoff_tests;
