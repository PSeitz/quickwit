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

use quickwit_common::metrics::IN_FLIGHT_INGEST_ROUTER;
use quickwit_common::tower::GrpcMetricsLayer;
use quickwit_metrics::{Counter, Gauge, LazyCounter, counter, lazy_counter};
use quickwit_proto::ingest::router::IngestFailureReason;
use quickwit_proto::types::AvailabilityZone;

use crate::IngestServiceError;
use crate::router_client::{IngestFailure, IngestResponse};

// Identical name and labels share the v2 counters through quickwit-metrics registration.
static INGEST_RESULT_TOTAL: LazyCounter = lazy_counter!(
    name: "ingest_result_total",
    description: "Number of ingest requests by result",
    subsystem: "ingest",
);

static INGEST_ATTEMPTS: LazyCounter = lazy_counter!(
    name: "ingest_attempts",
    description: "Number of routing attempts by AZ locality",
    subsystem: "ingest",
);

/// Final subrequest results, destination attempts by AZ, router request bytes, and client RPCs.
#[derive(Clone, Debug)]
pub(super) struct RouterMetrics {
    pub(super) results: Counter,
    pub(super) attempts: Counter,
    pub(super) in_flight_bytes: Gauge,
    pub(super) grpc_client: GrpcMetricsLayer,
}

impl Default for RouterMetrics {
    fn default() -> Self {
        Self {
            results: INGEST_RESULT_TOTAL.clone(),
            attempts: INGEST_ATTEMPTS.clone(),
            in_flight_bytes: IN_FLIGHT_INGEST_ROUTER.clone(),
            grpc_client: GrpcMetricsLayer::new("ingesthree", "client"),
        }
    }
}

impl RouterMetrics {
    pub(super) fn record_response(&self, response: &IngestResponse) {
        counter!(parent: self.results, "result" => "success")
            .inc_by(response.successes.len() as u64);
        for failure in &response.failures {
            let result = match failure {
                IngestFailure::V2(failure) => match failure.reason() {
                    IngestFailureReason::Unspecified => "unspecified",
                    IngestFailureReason::IndexNotFound => "index_not_found",
                    IngestFailureReason::SourceNotFound => "source_not_found",
                    IngestFailureReason::Internal => "internal",
                    IngestFailureReason::NoShardsAvailable => "no_shards_available",
                    IngestFailureReason::ShardRateLimited => "shard_rate_limited",
                    IngestFailureReason::WalFull => "wal_full",
                    IngestFailureReason::Timeout => "timeout",
                    IngestFailureReason::RouterLoadShedding => "router_load_shedding",
                    IngestFailureReason::LoadShedding => "load_shedding",
                    IngestFailureReason::CircuitBreaker => "circuit_breaker",
                },
                IngestFailure::Ingesthree { error, .. } => error_result(error),
            };
            counter!(parent: self.results, "result" => result).inc();
        }
    }

    pub(super) fn record_error(&self, error: &IngestServiceError, num_subrequests: usize) {
        counter!(parent: self.results, "result" => error_result(error))
            .inc_by(num_subrequests as u64);
    }

    pub(super) fn record_attempt(
        &self,
        router_zone: Option<&AvailabilityZone>,
        destination_zone: Option<&AvailabilityZone>,
    ) {
        let az_routing = match (router_zone, destination_zone) {
            (Some(router_zone), Some(destination_zone)) if router_zone == destination_zone => {
                "same_az"
            }
            (Some(_), Some(_)) => "cross_az",
            _ => "az_unaware",
        };
        counter!(parent: self.attempts, "az_routing" => az_routing).inc();
    }
}

fn error_result(error: &IngestServiceError) -> &'static str {
    match error {
        IngestServiceError::IndexNotFound { .. } => "index_not_found",
        IngestServiceError::BadRequest(_) | IngestServiceError::InvalidPosition(_) => "bad_request",
        IngestServiceError::RateLimited(_) => "load_shedding",
        IngestServiceError::Unavailable(_) => "unavailable",
        IngestServiceError::Corruption(_)
        | IngestServiceError::IndexAlreadyExists { .. }
        | IngestServiceError::Internal(_)
        | IngestServiceError::IoError(_) => "internal",
    }
}

#[cfg(test)]
impl RouterMetrics {
    pub(super) fn for_test(test: &'static str) -> Self {
        Self {
            results: counter!(parent: INGEST_RESULT_TOTAL, "test" => test),
            attempts: counter!(parent: INGEST_ATTEMPTS, "test" => test),
            in_flight_bytes: Gauge::local(),
            grpc_client: GrpcMetricsLayer::new_with_labels(
                "ingesthree",
                "client",
                quickwit_metrics::labels!("test" => test),
            ),
        }
    }

    pub(super) fn result_count(&self, result: &'static str) -> u64 {
        counter!(parent: self.results, "result" => result).get()
    }

    pub(super) fn attempt_count(&self, az_routing: &'static str) -> u64 {
        counter!(parent: self.attempts, "az_routing" => az_routing).get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ingesthree_counters_share_registration() {
        // Reproduce v2's independent declaration and child label, without accessing its module.
        let v2_results = counter!(
            name: "ingest_result_total",
            description: "Number of ingest requests by result",
            subsystem: "ingest",
        );
        let v2_success = counter!(parent: v2_results, "result" => "success");
        let metrics = RouterMetrics::default();
        assert_eq!(
            v2_success,
            counter!(parent: metrics.results, "result" => "success")
        );
        let v2_attempts = counter!(
            name: "ingest_attempts",
            description: "Number of routing attempts by AZ locality",
            subsystem: "ingest",
        );
        assert_eq!(
            counter!(parent: v2_attempts, "az_routing" => "same_az"),
            counter!(parent: metrics.attempts, "az_routing" => "same_az"),
        );
        assert_eq!(metrics.in_flight_bytes, *IN_FLIGHT_INGEST_ROUTER);
    }

    #[test]
    fn test_ingesthree_results_count_subrequests_including_all_invalid_batches() {
        use quickwit_proto::ingest::router::IngestSuccess;
        use quickwit_proto::ingest::{ParseFailure, ParseFailureReason};

        let metrics = RouterMetrics::for_test("subrequest_results");
        let response = IngestResponse {
            successes: vec![
                IngestSuccess {
                    num_ingested_docs: 50,
                    ..Default::default()
                },
                IngestSuccess {
                    num_ingested_docs: 0,
                    parse_failures: vec![ParseFailure {
                        reason: ParseFailureReason::InvalidJson as i32,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            failures: Vec::new(),
        };
        metrics.record_response(&response);
        assert_eq!(metrics.result_count("success"), 2);
    }

    #[test]
    fn test_ingesthree_attempt_az_labels() {
        let metrics = RouterMetrics::for_test("attempt_az_labels");
        let zone_a: AvailabilityZone = "zone-a".into();
        let zone_b: AvailabilityZone = "zone-b".into();
        metrics.record_attempt(Some(&zone_a), Some(&zone_a));
        metrics.record_attempt(Some(&zone_a), Some(&zone_b));
        metrics.record_attempt(None, Some(&zone_a));
        metrics.record_attempt(Some(&zone_a), None);
        metrics.record_attempt(None, None);
        assert_eq!(metrics.attempt_count("same_az"), 1);
        assert_eq!(metrics.attempt_count("cross_az"), 1);
        assert_eq!(metrics.attempt_count("az_unaware"), 3);
    }
}
