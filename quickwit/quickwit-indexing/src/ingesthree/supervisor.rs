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

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt;
use quickwit_metrics::GaugeGuard;
use quickwit_proto::metastore::AssignFilesForIndexing;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::error;

use super::S3Indexer;
use super::metrics::{SUPERVISOR_METRICS, SupervisorMetrics};

const IDLE_MIN: Duration = Duration::from_millis(100);
const IDLE_MAX: Duration = Duration::from_secs(5);
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);

/// Stops file polling and waits for all worker tasks to exit. Send with `Mailbox::ask`
/// before shutting down the actor universe. In-flight assignments remain retryable.
/// Blocking indexing work observes cancellation between operations and retains its scratch root.
#[derive(Debug)]
pub struct StopS3Indexers;

/// Owns the polling task, including its restart loop. Aborting this task cancels both
/// indexing and restart delays. Dropping the service also stops the worker.
pub(crate) struct S3IndexerSupervisor {
    task: JoinHandle<()>,
    _configured: GaugeGuard,
}

impl S3IndexerSupervisor {
    pub fn spawn(worker: S3Indexer, max_num_bytes: u64, max_wait_secs: u32) -> Self {
        Self::supervise(
            move || run(worker.clone(), max_num_bytes, max_wait_secs),
            SUPERVISOR_METRICS.clone(),
        )
    }

    fn supervise<F, Fut>(mut run: F, metrics: SupervisorMetrics) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send,
    {
        let configured = GaugeGuard::new(&metrics.configured, 1.0);
        let task = tokio::spawn(async move {
            let _live = GaugeGuard::new(&metrics.live, 1.0);
            let mut delay = RESTART_MIN;
            loop {
                let started = Instant::now();
                match AssertUnwindSafe(run()).catch_unwind().await {
                    Ok(Err(error)) => {
                        metrics.errors.inc();
                        error!(error = %format!("{error:#}"), "S3 indexer failed, restarting");
                    }
                    Ok(Ok(())) => {
                        metrics.unexpected_exits.inc();
                        error!("S3 indexer exited unexpectedly, restarting");
                    }
                    Err(_) => {
                        metrics.panics.inc();
                        error!("S3 indexer panicked, restarting");
                    }
                }
                if started.elapsed() >= RESTART_MAX {
                    delay = RESTART_MIN;
                }
                tokio::time::sleep(delay).await;
                metrics.restarts.inc();
                delay = (delay * 2).min(RESTART_MAX);
            }
        });
        Self {
            task,
            _configured: configured,
        }
    }

    pub async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for S3IndexerSupervisor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(mut worker: S3Indexer, max_num_bytes: u64, max_wait_secs: u32) -> anyhow::Result<()> {
    let mut idle_delay = IDLE_MIN;
    loop {
        // Claim only after the byte target or registration-age threshold is reached.
        // Include the file crossing the byte target, subject to the 1,000-file limit.
        let assignment = AssignFilesForIndexing {
            max_num_files: 1_000,
            max_num_bytes,
            retry_after_secs: 60,
            max_wait_secs,
        };
        match worker.index_next(assignment).await? {
            Some(_) => idle_delay = IDLE_MIN,
            None => {
                tokio::time::sleep(idle_delay).await;
                idle_delay = (idle_delay * 2).min(IDLE_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use quickwit_common::temp_dir::TempDirectory;
    use quickwit_proto::metastore::MockMetastoreService;
    use quickwit_proto::types::NodeId;
    use quickwit_storage::StorageResolver;

    use super::*;
    use crate::IndexingSplitCache;

    #[tokio::test(start_paused = true)]
    async fn errors_panics_and_unexpected_exits_restart_with_backoff() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let metrics = SupervisorMetrics::for_test();
        let supervisor = S3IndexerSupervisor::supervise(
            move || {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    match attempt {
                        0 => anyhow::bail!("injected error"),
                        1 => panic!("injected panic"),
                        _ => Ok(()),
                    }
                }
            },
            metrics.clone(),
        );
        assert_eq!(metrics.configured.get(), 1.0);
        tokio::task::yield_now().await;
        assert_eq!(metrics.live.get(), 1.0);
        assert_eq!(metrics.errors.get(), 1);
        assert_eq!(metrics.restarts.get(), 0);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        for (delay, expected) in [(1, 2), (2, 3), (4, 4)] {
            tokio::time::advance(Duration::from_secs(delay) - Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(attempts.load(Ordering::SeqCst), expected - 1);
            assert_eq!(metrics.configured.get(), 1.0);
            assert_eq!(metrics.live.get(), 1.0);
            assert_eq!(metrics.restarts.get(), (expected - 2) as u64);
            tokio::time::advance(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(attempts.load(Ordering::SeqCst), expected);
        }
        supervisor.stop().await;
        assert_eq!(metrics.configured.get(), 0.0);
        assert_eq!(metrics.live.get(), 0.0);
        assert_eq!(metrics.errors.get(), 1);
        assert_eq!(metrics.panics.get(), 1);
        assert_eq!(metrics.unexpected_exits.get(), 2);
        assert_eq!(metrics.restarts.get(), 3);
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(metrics.restarts.get(), 3);
    }

    #[tokio::test]
    async fn stop_before_first_poll_releases_configured_supervisor() {
        let metrics = SupervisorMetrics::for_test();
        let supervisor = S3IndexerSupervisor::supervise(
            std::future::pending::<anyhow::Result<()>>,
            metrics.clone(),
        );
        assert_eq!(metrics.configured.get(), 1.0);
        assert_eq!(metrics.live.get(), 0.0);
        supervisor.stop().await;
        assert_eq!(metrics.configured.get(), 0.0);
        assert_eq!(metrics.live.get(), 0.0);
        assert_eq!(metrics.restarts.get(), 0);
        assert_eq!(metrics.errors.get(), 0);
        assert_eq!(metrics.panics.get(), 0);
        assert_eq!(metrics.unexpected_exits.get(), 0);
    }

    #[tokio::test]
    async fn stop_cancels_in_flight_work_and_drop_stops_the_task() {
        struct Work(Arc<AtomicUsize>);
        impl Drop for Work {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let cancelled = Arc::new(AtomicUsize::new(0));
        for explicit_stop in [true, false] {
            let metrics = SupervisorMetrics::for_test();
            let count = cancelled.clone();
            let supervisor = S3IndexerSupervisor::supervise(
                move || {
                    let work = Work(count.clone());
                    async move {
                        let _work = work;
                        std::future::pending().await
                    }
                },
                metrics.clone(),
            );
            tokio::task::yield_now().await;
            assert_eq!(metrics.configured.get(), 1.0);
            assert_eq!(metrics.live.get(), 1.0);
            if explicit_stop {
                supervisor.stop().await;
                assert_eq!(cancelled.load(Ordering::SeqCst), 1);
            } else {
                drop(supervisor);
                tokio::task::yield_now().await;
                assert_eq!(cancelled.load(Ordering::SeqCst), 2);
            }
            assert_eq!(metrics.configured.get(), 0.0);
            assert_eq!(metrics.live.get(), 0.0);
            assert_eq!(metrics.errors.get(), 0);
            assert_eq!(metrics.panics.get(), 0);
            assert_eq!(metrics.unexpected_exits.get(), 0);
            assert_eq!(metrics.restarts.get(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn empty_assignments_poll_with_idle_backoff_and_stop() {
        let polls = Arc::new(AtomicUsize::new(0));
        let count = polls.clone();
        let mut metastore = MockMetastoreService::new();
        metastore
            .expect_assign_files_for_indexing()
            .returning(move |request| {
                assert_eq!(request.max_num_bytes, 100_000_000);
                assert_eq!(request.max_wait_secs, 15);
                count.fetch_add(1, Ordering::SeqCst);
                Ok(Default::default())
            });
        let worker = S3Indexer {
            node_id: NodeId::from_str("test-node"),
            source_id: quickwit_config::INGEST_V2_SOURCE_ID.to_string(),
            metastore: quickwit_proto::metastore::MetastoreServiceClient::from_mock(metastore),
            storage_resolver: StorageResolver::for_test(),
            scratch_directory: TempDirectory::for_test(),
            split_cache: Arc::new(IndexingSplitCache::no_caching()),
            max_concurrent_split_uploads: 1,
            indexing_io_throughput_limiter: None,
            fingerprinter: None,
        };
        let metrics = SupervisorMetrics::for_test();
        let supervisor = S3IndexerSupervisor::supervise(
            move || run(worker.clone(), 100_000_000, 15),
            metrics.clone(),
        );
        tokio::task::yield_now().await;
        assert_eq!(metrics.configured.get(), 1.0);
        assert_eq!(metrics.live.get(), 1.0);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        tokio::time::advance(IDLE_MIN).await;
        tokio::task::yield_now().await;
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        tokio::time::advance(IDLE_MIN).await;
        tokio::task::yield_now().await;
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        tokio::time::advance(IDLE_MIN).await;
        tokio::task::yield_now().await;
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        supervisor.stop().await;
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        assert_eq!(metrics.configured.get(), 0.0);
        assert_eq!(metrics.live.get(), 0.0);
        assert_eq!(metrics.restarts.get(), 0);
    }
}
