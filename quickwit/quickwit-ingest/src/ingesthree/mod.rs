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

//! Opt-in ingesthree file intake, routing, and local storage.
//!
//! [`WriteQueue`] queues document batches for validation and writing through
//! [`Ingester`]. It enforces queue and disk byte limits, finishes files by size
//! and age, and limits open files. An optional [`Uploader`] hands finished files to
//! storage and the metastore before the writer deletes them and releases disk charges.

mod documents_staged_for_upload;
#[cfg(test)]
mod file_size_tests;
mod handoff;
mod metadata_cache;
mod router;
mod service;
#[cfg(test)]
mod tests;
mod write_queue;

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use bytes::Bytes;
use bytesize::ByteSize;
use quickwit_proto::types::IndexUid;
use tokio::time::Instant;
use ulid::Ulid;

pub use self::documents_staged_for_upload::{DocumentFileReader, DocumentsStagedForUpload};
use self::documents_staged_for_upload::{
    MAGIC, encode_metadata, record_size, recover, write_record,
};
pub use self::handoff::Uploader;
pub use self::metadata_cache::METADATA_CHANGE_KEY;
pub use self::router::IngesthreeRouter;
pub use self::service::IngesthreeService;
pub use self::write_queue::{AppendResponse, IngestError, WriteQueue};

struct ActiveFile {
    input: DocumentsStagedForUpload,
    finish_at: Instant,
    writer: BufWriter<File>,
}

const TARGET_FILE_SIZE_ENV_KEY: &str = "QW_INGESTHREE_TARGET_FILE_SIZE";

// File-finishing thresholds and the active-writer cap.
struct FileLimits {
    target_file_bytes: u64,
    max_file_age: Duration,
    max_open_files: usize,
}

impl Default for FileLimits {
    fn default() -> Self {
        Self {
            target_file_bytes: 100_000_000,
            max_file_age: Duration::from_secs(10),
            max_open_files: 1_024,
        }
    }
}

impl FileLimits {
    fn from_env() -> anyhow::Result<Self> {
        let mut limits = Self::default();
        match std::env::var(TARGET_FILE_SIZE_ENV_KEY) {
            Ok(value) => {
                limits.target_file_bytes = value
                    .parse::<ByteSize>()
                    .map_err(|error| anyhow::anyhow!("{TARGET_FILE_SIZE_ENV_KEY}: {error}"))?
                    .as_u64();
                ensure!(
                    limits.target_file_bytes > 0,
                    "{TARGET_FILE_SIZE_ENV_KEY} must be greater than zero"
                );
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {TARGET_FILE_SIZE_ENV_KEY}"));
            }
        }
        Ok(limits)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("ingesthree open-file limit reached")]
struct OpenFileLimit;

/// Accumulates documents in per-index files under `<data_dir>/ingesthree`.
///
/// Successful appends have reached the kernel. No fsync or host-crash durability
/// is promised. One owner holds the directory lock through writing and recovery.
/// File transitions use atomic renames into unused paths, relying on unique file
/// IDs and exclusive directory ownership.
/// An IO failure stops further writes until reopen/recovery, so later successful
/// appends can never appear after an unrepaired partial record.
pub struct Ingester {
    directory: PathBuf,
    active: HashMap<IndexUid, ActiveFile>,
    finished: Vec<DocumentsStagedForUpload>,
    file_limits: FileLimits,
    max_disk_bytes: u64,
    disk_bytes: u64,
    requires_recovery: bool,
    // Drop after the writers, whose destructors may flush buffers. The async writer
    // also retains this lock until it has dropped its handoff future.
    directory_lock: Arc<File>,
}

impl Ingester {
    /// Recovers surviving inputs under their original IDs and finishes them. New
    /// writes always use fresh files. `max_disk_bytes` limits logical input bytes,
    /// including framing; filesystem allocation/metadata and drain reserve must
    /// be budgeted separately by the caller. Existing data is retained even if
    /// it already exceeds this limit.
    pub fn open(data_dir: &Path, max_disk_bytes: u64) -> anyhow::Result<Self> {
        let file_limits = FileLimits::from_env()?;
        let directory = data_dir.join("ingesthree");
        fs::create_dir_all(&directory)?;
        let directory_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("LOCK"))?;
        directory_lock
            .try_lock()
            .context("failed to exclusively lock ingesthree directory")?;
        let mut finished = Vec::new();
        let mut disk_bytes = 0u64;
        // Snapshot before renaming/removing entries: live directory iteration
        // during mutation can return the same file twice on some filesystems.
        let entries = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
        for entry in entries {
            ensure!(
                entry.file_type()?.is_file(),
                "unexpected input directory entry"
            );
            let path = entry.path();
            if entry.file_name() == "LOCK" {
                continue;
            }
            let file_id: Ulid = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("invalid input filename")?
                .parse()?;
            let extension = path.extension().and_then(|extension| extension.to_str());
            if extension == Some("creating") {
                // Only headers are written before the rename to `.open`. No
                // documents could have been acknowledged from this file.
                fs::remove_file(path)?;
                continue;
            }
            let is_finished = extension == Some("finished");
            ensure!(
                is_finished || extension == Some("open"),
                "unknown input file state"
            );
            let mut input = recover(&path, file_id, is_finished)
                .with_context(|| format!("failed to recover input {}", path.display()))?;
            if input.num_docs == 0 {
                ensure!(!is_finished, "finished input contains no documents");
                fs::remove_file(path)?;
                continue;
            }
            if !is_finished {
                let finished_path = path.with_extension("finished");
                fs::rename(&path, &finished_path)?;
                input.path = finished_path;
            }
            disk_bytes = disk_bytes
                .checked_add(input.num_bytes)
                .context("input size overflow")?;
            finished.push(input);
        }
        finished.sort_by_key(|input| input.file_id);
        Ok(Self {
            directory,
            directory_lock: Arc::new(directory_lock),
            active: HashMap::new(),
            finished,
            file_limits,
            max_disk_bytes,
            disk_bytes,
            requires_recovery: false,
        })
    }

    /// Appends opaque documents already validated by the caller. Empty batches
    /// are rejected without creating a file. An error can leave unacknowledged
    /// records on disk; retrying is at-least-once, not request deduplication.
    /// Files reaching the size target are finished after a whole batch, which can exceed the
    /// target. WriteQueue's writer loop finishes expired files between requests.
    /// At the open-file limit, requests needing an additional writer are rejected.
    pub fn append(&mut self, index_uid: &IndexUid, documents: &[Bytes]) -> anyhow::Result<Ulid> {
        ensure!(
            !self.requires_recovery,
            "ingester requires recovery after an IO failure"
        );
        ensure!(!documents.is_empty(), "empty document batch");
        let mut added_bytes = 0u64;
        for document in documents {
            added_bytes = added_bytes
                .checked_add(record_size(document.len())?)
                .context("input size overflow")?;
        }
        let new_header = if self.active.contains_key(index_uid) {
            None
        } else {
            ensure!(
                self.active.len() < self.file_limits.max_open_files,
                OpenFileLimit
            );
            let header = encode_metadata(index_uid)?;
            added_bytes = added_bytes
                .checked_add(MAGIC.len() as u64 + record_size(header.len())?)
                .context("input size overflow")?;
            Some(header)
        };
        let disk_bytes = self
            .disk_bytes
            .checked_add(added_bytes)
            .context("input size overflow")?;
        ensure!(
            disk_bytes <= self.max_disk_bytes,
            "ingesthree local storage budget exhausted"
        );
        // Set before creation/append IO. Validation and budget rejection leave
        // the ingester usable.
        self.requires_recovery = true;
        if let Some(header) = new_header {
            let file_id = Ulid::new();
            let creating_path = self.directory.join(format!("{file_id}.creating"));
            let open_path = creating_path.with_extension("open");
            let finish_at = Instant::now() + self.file_limits.max_file_age;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&creating_path)?;
            let mut writer = BufWriter::new(file);
            writer.write_all(MAGIC)?;
            write_record(&mut writer, &header)?;
            writer.flush()?;
            fs::rename(&creating_path, &open_path)?;
            self.active.insert(
                index_uid.clone(),
                ActiveFile {
                    input: DocumentsStagedForUpload {
                        file_id,
                        index_uid: index_uid.clone(),
                        path: open_path,
                        num_bytes: MAGIC.len() as u64 + record_size(header.len())?,
                        num_docs: 0,
                    },
                    finish_at,
                    writer,
                },
            );
        }
        let active = self
            .active
            .get_mut(index_uid)
            .expect("active file just created");
        for document in documents {
            write_record(&mut active.writer, document)?;
            active.input.num_bytes += record_size(document.len())?;
            active.input.num_docs += 1;
        }
        // This is the acknowledgment boundary, including for small writes that
        // fit entirely inside BufWriter. flush is deliberately distinct from fsync.
        active.writer.flush()?;
        let file_id = active.input.file_id;
        let reached_target = active.input.num_bytes >= self.file_limits.target_file_bytes;
        self.disk_bytes = disk_bytes;
        self.requires_recovery = false;
        if reached_target {
            self.finish_file(index_uid)?;
        }
        Ok(file_id)
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.active.values().map(|active| active.finish_at).min()
    }

    fn finish_expired_files(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let expired: Vec<IndexUid> = self
            .active
            .iter()
            .filter(|(_, active)| active.finish_at <= now)
            .map(|(index_uid, _)| index_uid.clone())
            .collect();
        for index_uid in expired {
            self.finish_file(&index_uid)?;
        }
        Ok(())
    }

    /// Finishes this index's current file without changing its contents. A later
    /// append starts a fresh file. With no active file this is a no-op.
    pub fn finish_file(&mut self, index_uid: &IndexUid) -> anyhow::Result<()> {
        ensure!(
            !self.requires_recovery,
            "ingester requires recovery after an IO failure"
        );
        let Some(mut active) = self.active.remove(index_uid) else {
            return Ok(());
        };
        self.requires_recovery = true;
        active.writer.flush()?;
        let path = active.input.path.with_extension("finished");
        fs::rename(&active.input.path, &path)?;
        active.input.path = path;
        self.finished.push(active.input);
        self.requires_recovery = false;
        Ok(())
    }

    pub fn finished_files(&self) -> &[DocumentsStagedForUpload] {
        &self.finished
    }

    /// Called after registration or remote cleanup of an input whose index is gone.
    /// Deletion and accounting run together on the blocking pool under the directory lock.
    fn delete_finished_file(&mut self, file_id: Ulid) -> anyhow::Result<u64> {
        let position = self
            .finished
            .iter()
            .position(|input| input.file_id == file_id)
            .context("finished file is no longer retained locally")?;
        let input = &self.finished[position];
        fs::remove_file(&input.path)?;
        let num_bytes = input.num_bytes;
        self.disk_bytes -= num_bytes;
        self.finished.remove(position);
        Ok(num_bytes)
    }

    /// Stops local intake and finishes all files, including small ones. This does
    /// not perform upload/registration and therefore is not full decommissioning.
    /// All local files remain on disk.
    pub fn finish(mut self) -> anyhow::Result<Vec<DocumentsStagedForUpload>> {
        self.finish_files()?;
        Ok(self.finished)
    }

    /// Finishes active files while keeping directory ownership for handoff and deletion.
    fn finish_files(&mut self) -> anyhow::Result<()> {
        ensure!(
            !self.requires_recovery,
            "ingester requires recovery after an IO failure"
        );
        let indexes: Vec<IndexUid> = self.active.keys().cloned().collect();
        for index_uid in indexes {
            self.finish_file(&index_uid)?;
        }
        Ok(())
    }
}
