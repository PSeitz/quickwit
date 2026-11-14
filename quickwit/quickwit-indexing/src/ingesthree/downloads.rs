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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, ensure};
use quickwit_common::uri::Uri;
use quickwit_proto::metastore::DocumentsStagedForUpload;
use quickwit_storage::{OwnedBytes, StorageResolver};
use tokio::sync::{Notify, mpsc};

const DOWNLOAD_TARGET_BYTES: u64 = 100_000_000;

/// One sequential downloader reserves bytes before each GET. Indexing releases the reservation
/// when it takes a file. This single-producer contract permits at most one file to cross the
/// target.
#[derive(Default)]
struct DownloadBudget {
    bytes: AtomicU64,
    released: Notify,
}

impl DownloadBudget {
    async fn reserve(self: &Arc<Self>, num_bytes: u64) -> DownloadReservation {
        loop {
            let released = self.released.notified();
            if self.bytes.load(Ordering::Relaxed) < DOWNLOAD_TARGET_BYTES {
                self.bytes.fetch_add(num_bytes, Ordering::Relaxed);
                return DownloadReservation {
                    budget: self.clone(),
                    num_bytes,
                };
            }
            released.await;
        }
    }
}

struct DownloadReservation {
    budget: Arc<DownloadBudget>,
    num_bytes: u64,
}

impl Drop for DownloadReservation {
    fn drop(&mut self) {
        self.budget
            .bytes
            .fetch_sub(self.num_bytes, Ordering::Relaxed);
        self.budget.released.notify_one();
    }
}

/// Field order releases buffered bytes before their reservation when a queued file is discarded.
pub(super) struct DownloadedFile {
    contents: OwnedBytes,
    reservation: DownloadReservation,
}

impl DownloadedFile {
    pub fn into_contents(self) -> OwnedBytes {
        let Self {
            contents,
            reservation,
        } = self;
        // The active input belongs to indexing. Allow the downloader to refill while it is
        // consumed.
        drop(reservation);
        contents
    }
}

/// Reports download failures through the same channel as files. Closing the channel signals that
/// all files were sent. A closed receiver means indexing exited, so outstanding downloads stop.
pub(super) async fn download_files(
    files: &[DocumentsStagedForUpload],
    resolver: &StorageResolver,
    sender: mpsc::UnboundedSender<anyhow::Result<DownloadedFile>>,
) {
    let downloading = async {
        let budget = Arc::new(DownloadBudget::default());
        for file in files {
            let reservation = budget.reserve(file.num_bytes).await;
            let uri: Uri = file.file_uri.parse()?;
            let storage = resolver
                .resolve(&uri.parent().context("input URI has no parent")?)
                .await?;
            let contents = storage
                .get_all(uri.file_name().context("input URI has no filename")?)
                .await?;
            ensure!(
                contents.len() as u64 == file.num_bytes,
                "downloaded input size differs from registration"
            );
            sender
                .send(Ok(DownloadedFile {
                    contents,
                    reservation,
                }))
                .map_err(|_| anyhow::anyhow!("indexing stopped before receiving input"))?;
        }
        anyhow::Ok(())
    };
    let result = tokio::select! {
        _ = sender.closed() => return,
        result = downloading => result,
    };
    if let Err(error) = result {
        // If indexing already exited, its own error is authoritative and no receiver remains.
        let _ = sender.send(Err(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_download_budget_overshoots_by_one_file() {
        let budget = Arc::new(DownloadBudget::default());
        let first = budget.reserve(90_000_000).await;
        let second = budget.reserve(20_000_000).await;
        assert_eq!(budget.bytes.load(Ordering::Relaxed), 110_000_000);
        let next = budget.reserve(1);
        tokio::pin!(next);
        assert!(futures::poll!(&mut next).is_pending());
        drop(first);
        let third = next.await;
        assert_eq!(budget.bytes.load(Ordering::Relaxed), 20_000_001);
        drop((second, third));
        assert_eq!(budget.bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_indexing_owns_active_input_outside_download_budget() {
        let budget = Arc::new(DownloadBudget::default());
        let file = DownloadedFile {
            contents: OwnedBytes::new(vec![1, 2, 3]),
            reservation: budget.reserve(DOWNLOAD_TARGET_BYTES + 1).await,
        };
        let next = budget.reserve(1);
        tokio::pin!(next);
        assert!(futures::poll!(&mut next).is_pending());
        let contents = file.into_contents();
        assert_eq!(budget.bytes.load(Ordering::Relaxed), 0);
        let queued = DownloadedFile {
            contents: OwnedBytes::new(vec![4]),
            reservation: next.await,
        };
        assert_eq!(budget.bytes.load(Ordering::Relaxed), 1);
        assert_eq!(contents.as_slice(), &[1, 2, 3]);
        drop(queued);
        assert_eq!(budget.bytes.load(Ordering::Relaxed), 0);
    }
}
