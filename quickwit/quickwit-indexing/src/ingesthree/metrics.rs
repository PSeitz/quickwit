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

use std::sync::LazyLock;

use quickwit_metrics::{Counter, Gauge, LazyGauge, counter, gauge, lazy_gauge};

pub(super) static ACTIVE_ASSIGNMENTS: LazyGauge = lazy_gauge!(
    name: "ingesthree_active_assignments",
    description: "Assigned groups being indexed or published, including blocking work awaiting cancellation.",
    subsystem: "indexing",
);

/// Metrics for configured worker supervisors, their live tasks, failures, and restarts.
/// Supervisor counts include idle polling and restart backoff.
#[derive(Clone)]
pub(super) struct SupervisorMetrics {
    pub configured: Gauge,
    pub live: Gauge,
    pub errors: Counter,
    pub panics: Counter,
    pub unexpected_exits: Counter,
    pub restarts: Counter,
}

pub(super) static SUPERVISOR_METRICS: LazyLock<SupervisorMetrics> = LazyLock::new(|| {
    let failures = counter!(
        name: "ingesthree_worker_failures_total",
        description: "Worker runs ending with an error, panic, or unexpected exit, by reason.",
        subsystem: "indexing",
    );
    SupervisorMetrics {
        configured: gauge!(
            name: "ingesthree_configured_supervisors",
            description: "Worker supervisors owned by the indexing service, including restart backoff.",
            subsystem: "indexing",
        ),
        live: gauge!(
            name: "ingesthree_live_supervisors",
            description: "Live worker supervisor tasks, including idle polling and restart backoff.",
            subsystem: "indexing",
        ),
        errors: counter!(parent: failures, "reason" => "error"),
        panics: counter!(parent: failures, "reason" => "panic"),
        unexpected_exits: counter!(parent: failures, "reason" => "unexpected_exit"),
        restarts: counter!(
            name: "ingesthree_worker_restarts_total",
            description: "Worker restarts begun after failure backoff.",
            subsystem: "indexing",
        ),
    }
});

#[cfg(test)]
impl SupervisorMetrics {
    pub fn for_test() -> Self {
        Self {
            configured: Gauge::local(),
            live: Gauge::local(),
            errors: Counter::local(),
            panics: Counter::local(),
            unexpected_exits: Counter::local(),
            restarts: Counter::local(),
        }
    }
}
