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

use quickwit_common::metrics::IN_FLIGHT_INGESTHREE_QUEUE;
use quickwit_metrics::{Counter, Gauge, GaugeGuard, counter, gauge, label_values};
use quickwit_proto::ingest::DocBatchV2;

use super::IngestError;
use crate::metrics::{DOCS_BYTES_TOTAL, DOCS_TOTAL, VALIDITY};

// Documents and framed batch bytes measured after validation, before file append.
#[derive(Clone)]
pub(super) struct ValidationMetrics {
    pub valid_docs: Counter,
    pub invalid_docs: Counter,
    pub valid_bytes: Counter,
    pub invalid_bytes: Counter,
}

impl Default for ValidationMetrics {
    fn default() -> Self {
        Self {
            valid_docs: counter!(parent: DOCS_TOTAL, labels: [label_values!(VALIDITY => "valid")]),
            invalid_docs: counter!(parent: DOCS_TOTAL, labels: [label_values!(VALIDITY => "invalid")]),
            valid_bytes: counter!(parent: DOCS_BYTES_TOTAL, labels: [label_values!(VALIDITY => "valid")]),
            invalid_bytes: counter!(parent: DOCS_BYTES_TOTAL, labels: [label_values!(VALIDITY => "invalid")]),
        }
    }
}

impl ValidationMetrics {
    pub fn record(&self, batch: &DocBatchV2, num_invalid_docs: usize, original_bytes: usize) {
        self.valid_docs.inc_by(batch.num_docs() as u64);
        self.valid_bytes.inc_by(batch.num_bytes() as u64);
        self.invalid_docs.inc_by(num_invalid_docs as u64);
        self.invalid_bytes
            .inc_by((original_bytes - batch.num_bytes()) as u64);
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            valid_docs: Counter::local(),
            invalid_docs: Counter::local(),
            valid_bytes: Counter::local(),
            invalid_bytes: Counter::local(),
        }
    }
}

// Input reservations are owned contributions. Disk bytes and configured limits
// are node-wide snapshots for the node's ingesthree directory. Disk bytes survive
// writer shutdown because files remain on disk. Recovery replaces that snapshot.
// Mutations hold the admission lock.
pub(super) struct QueueMetrics {
    pub queue_bytes: GaugeGuard,
    pub memory_bytes: GaugeGuard,
    pub disk_bytes: Gauge,
    pub disk_reserved_bytes: GaugeGuard,
    pub queue_limit_bytes: Gauge,
    pub disk_limit_bytes: Gauge,
    pub capacity_score: GaugeGuard,
    pub validation: ValidationMetrics,
    pub queue_full: Counter,
    pub storage_full: Counter,
    pub open_file_limit: Counter,
    pub unavailable: Counter,
}

impl QueueMetrics {
    pub fn new(max_queue_bytes: u64, max_disk_bytes: u64, disk_bytes: u64) -> Self {
        let rejections = counter!(
            name: "admission_rejections_total",
            description: "Append admission rejections by reason, counted at the rejecting destination.",
            subsystem: "ingesthree",
        );
        let metrics = Self {
            queue_bytes: GaugeGuard::new(
                &gauge!(
                    name: "queue_bytes",
                    description: "Reserved bytes for queued and processing input, including validation copies and descriptors.",
                    subsystem: "ingesthree",
                ),
                0.0,
            ),
            memory_bytes: GaugeGuard::new(&IN_FLIGHT_INGESTHREE_QUEUE, 0.0),
            disk_bytes: gauge!(
                name: "disk_bytes",
                description: "Logical framed-file bytes retained in the node's ingesthree directory, including recovered files, released after local deletion.",
                subsystem: "ingesthree",
            ),
            disk_reserved_bytes: GaugeGuard::new(
                &gauge!(
                    name: "disk_reserved_bytes",
                    description: "Disk bytes reserved for queued and processing appends, including a possible new file header.",
                    subsystem: "ingesthree",
                ),
                0.0,
            ),
            queue_limit_bytes: gauge!(
                name: "queue_limit_bytes",
                description: "Configured queue byte budget of the node's ingesthree writer.",
                subsystem: "ingesthree",
            ),
            disk_limit_bytes: gauge!(
                name: "disk_limit_bytes",
                description: "Configured logical disk byte budget of the node's ingesthree writer.",
                subsystem: "ingesthree",
            ),
            capacity_score: GaugeGuard::new(
                &gauge!(
                    name: "capacity_score",
                    description: "Last sampled admission capacity score (0-10), zero before sampling and after intake closes. One writer per node.",
                    subsystem: "ingesthree",
                ),
                0.0,
            ),
            validation: ValidationMetrics::default(),
            queue_full: counter!(parent: rejections, "reason" => "queue_full"),
            storage_full: counter!(parent: rejections, "reason" => "storage_full"),
            open_file_limit: counter!(parent: rejections, "reason" => "open_file_limit"),
            unavailable: counter!(parent: rejections, "reason" => "unavailable"),
        };
        metrics.disk_bytes.set(disk_bytes as f64);
        metrics.queue_limit_bytes.set(max_queue_bytes as f64);
        metrics.disk_limit_bytes.set(max_disk_bytes as f64);
        metrics
    }

    pub fn record_usage(&self, queue_bytes: u64, reserved_disk_bytes: u64) {
        set_contribution(&self.queue_bytes, queue_bytes as f64);
        set_contribution(&self.memory_bytes, queue_bytes as f64);
        set_contribution(&self.disk_reserved_bytes, reserved_disk_bytes as f64);
    }

    pub fn record_capacity(&self, score: usize) {
        set_contribution(&self.capacity_score, score as f64);
    }

    pub fn reject(&self, error: IngestError) -> IngestError {
        match &error {
            IngestError::QueueFull => self.queue_full.inc(),
            IngestError::StorageFull => self.storage_full.inc(),
            IngestError::OpenFileLimit => self.open_file_limit.inc(),
            IngestError::Unavailable => self.unavailable.inc(),
            _ => unreachable!("only admission rejections have a rejection counter"),
        }
        error
    }
}

fn set_contribution(guard: &GaugeGuard, value: f64) {
    guard.increment(value - guard.delta());
}
