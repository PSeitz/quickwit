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

use std::cmp::Reverse;
use std::time::Duration;

use bytesize::ByteSize;
use quickwit_cluster::{Cluster, ClusterNode};
use quickwit_common::rendezvous_hasher::node_affinity;
use quickwit_metrics::GaugeGuard;
use quickwit_proto::ingest::ingesthree::ingesthree_service_client::IngesthreeServiceClient;
use quickwit_proto::ingest::router::{
    IngestFailureReason, IngestRequestV2, IngestResponseV2, IngestSubrequest,
};
use quickwit_proto::ingest::{CommitTypeV2, RateLimitingCause};
use quickwit_proto::types::{AvailabilityZone, NodeId};
use tonic::{Code, Request, Status};
use tower::{Layer, Service};

#[path = "router_metrics.rs"]
mod metrics;

use metrics::RouterMetrics;

use super::service::{VERSION, VERSION_HEADER, failure, validate_request};
use crate::IngestServiceError;
use crate::router_client::{IngestFailure, IngestResponse};

/// Public REST/ES/OTLP router backed exclusively by ready ingesthree destinations.
/// A request snapshots discovery. Each destination is tried once, up to three attempts
/// per index batch. Transport failures can follow a write, so retries are at-least-once.
#[derive(Clone, Debug)]
pub struct IngesthreeRouter {
    cluster: Cluster,
    availability_zone: Option<AvailabilityZone>,
    max_grpc_message_size: ByteSize,
    metrics: RouterMetrics,
}

impl IngesthreeRouter {
    pub fn new(
        cluster: Cluster,
        availability_zone: Option<AvailabilityZone>,
        max_grpc_message_size: ByteSize,
    ) -> Self {
        Self {
            cluster,
            availability_zone,
            max_grpc_message_size,
            metrics: RouterMetrics::default(),
        }
    }

    pub async fn ingest(
        &self,
        request: IngestRequestV2,
    ) -> Result<IngestResponse, IngestServiceError> {
        let _request_bytes =
            GaugeGuard::new(&self.metrics.in_flight_bytes, request.num_bytes() as f64);
        if let Err(error) = validate_request(&request) {
            self.metrics.record_error(&error, request.subrequests.len());
            return Err(error);
        }
        let ready_nodes = self.cluster.ready_nodes().await;
        let mut response = IngestResponse::default();
        for subrequest in request.subrequests {
            let destinations = destinations(
                &ready_nodes,
                &subrequest.index_id,
                self.availability_zone.as_ref(),
            );
            let result = route_subrequest(
                &subrequest,
                destinations,
                self.max_grpc_message_size,
                self.availability_zone.as_ref(),
                &self.metrics,
                Some(&self.cluster.self_node_id()),
            )
            .await;
            response.successes.extend(result.successes);
            response.failures.extend(result.failures);
        }
        self.metrics.record_response(&response);
        Ok(response)
    }
}

fn destinations<'a>(
    nodes: &'a [ClusterNode],
    index_id: &str,
    availability_zone: Option<&AvailabilityZone>,
) -> Vec<&'a ClusterNode> {
    let mut destinations: Vec<_> = nodes
        .iter()
        .filter(|node| node.is_ready && node.is_ingesthree())
        .collect();
    destinations.sort_by_cached_key(|node| {
        let same_zone =
            availability_zone.is_some() && node.availability_zone().as_ref() == availability_zone;
        let affinity = node_affinity(&node.node_id, &index_id) as u128;
        (Reverse(same_zone), Reverse(affinity), node.node_id.clone())
    });
    destinations
}

/// Peer candidates are eligible and ordered by AZ, then index affinity.
/// Sample before applying capacity and affinity so a single index can use many nodes.
/// Remove the winner while preserving order, so retries always try distinct nodes.
fn take_destination<'a>(
    destinations: &mut Vec<&'a ClusterNode>,
    availability_zone: Option<&AvailabilityZone>,
    rng: &mut impl rand::Rng,
) -> Option<&'a ClusterNode> {
    if destinations.is_empty() {
        return None;
    }
    let same_zone_count = destinations
        .iter()
        .take_while(|node| {
            availability_zone.is_some() && node.availability_zone().as_ref() == availability_zone
        })
        .count();
    let num_candidates = if same_zone_count > 0 {
        same_zone_count
    } else {
        destinations.len()
    };
    let selected = rand::seq::index::sample(rng, num_candidates, num_candidates.min(2))
        .into_iter()
        .min_by_key(|&index| (Reverse(destinations[index].ingesthree_capacity()), index))
        .expect("at least one eligible destination");
    Some(destinations.remove(selected))
}

async fn route_subrequest(
    subrequest: &IngestSubrequest,
    mut destinations: Vec<&ClusterNode>,
    max_message_size: ByteSize,
    availability_zone: Option<&AvailabilityZone>,
    metrics: &RouterMetrics,
    local_node_id: Option<&NodeId>,
) -> IngestResponse {
    let mut error =
        IngestServiceError::Unavailable("no ready ingest destination accepted the request".into());
    // Filter capacity before sampling, without consuming retries.
    // Retain overload evidence just as we do for a destination's rejection below.
    destinations.retain(|node| match node.ingesthree_capacity() {
        Some(0) => {
            error = IngestServiceError::RateLimited(RateLimitingCause::LoadShedding);
            false
        }
        Some(_) => true,
        None => false,
    });
    // Prefer our own eligible intake before sampling peers. Removing it also prevents
    // retries from choosing it again after rejection. Discovery capacity is advisory.
    let mut local = local_node_id.and_then(|node_id| {
        let index = destinations
            .iter()
            .position(|node| &node.node_id == node_id)?;
        Some(destinations.remove(index))
    });
    for _ in 0..3 {
        let Some(node) = local
            .take()
            .or_else(|| take_destination(&mut destinations, availability_zone, &mut rand::rng()))
        else {
            break;
        };
        metrics.record_attempt(availability_zone, node.availability_zone().as_ref());
        let client = IngesthreeServiceClient::new(node.channel())
            .max_decoding_message_size(max_message_size.0 as usize)
            .max_encoding_message_size(max_message_size.0 as usize);
        let mut request = Request::new(IngestRequestV2 {
            subrequests: vec![subrequest.clone()],
            commit_type: CommitTypeV2::Auto as i32,
        });
        request
            .metadata_mut()
            .insert(VERSION_HEADER, VERSION.parse().unwrap());
        request.set_timeout(Duration::from_secs(5));
        let result = metrics
            .grpc_client
            .layer(tower::service_fn(move |request| {
                let mut client = client.clone();
                async move {
                    // Keep expiry inside the metrics layer and prioritize it if both
                    // timers have elapsed: tonic reports its transport timeout as Cancelled.
                    // Dropping the caller's future still counts as cancellation.
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {
                            Err(Status::deadline_exceeded("ingesthree intake timed out"))
                        }
                        response = client.ingest(request) => response,
                    }
                }
            }))
            .call(request)
            .await;
        let attempt_error = match result {
            Ok(response) => {
                let response = response.into_inner();
                if !valid_response(subrequest, &response) {
                    IngestServiceError::Internal("malformed ingesthree intake response".into())
                } else if matches!(response.failures.first(), Some(failure) if failure.reason() == IngestFailureReason::LoadShedding)
                {
                    IngestServiceError::RateLimited(RateLimitingCause::LoadShedding)
                } else {
                    return response.into();
                }
            }
            Err(status) => {
                tracing::warn!(node_id = %node.node_id, index_id = %subrequest.index_id, %status, "ingesthree intake failed");
                match status.code() {
                    Code::ResourceExhausted => {
                        IngestServiceError::RateLimited(RateLimitingCause::LoadShedding)
                    }
                    Code::Unavailable => IngestServiceError::Unavailable(status.message().into()),
                    Code::InvalidArgument => {
                        IngestServiceError::BadRequest(status.message().into())
                    }
                    // Preserve the existing timeout response and stop retrying.
                    Code::DeadlineExceeded => {
                        return IngestResponseV2 {
                            successes: Vec::new(),
                            failures: vec![failure(subrequest, IngestFailureReason::Timeout)],
                        }
                        .into();
                    }
                    _ => IngestServiceError::Internal(status.to_string()),
                }
            }
        };
        match attempt_error {
            IngestServiceError::Unavailable(_) => {
                // Preserve capacity evidence if remaining candidates are unavailable.
                if !matches!(error, IngestServiceError::RateLimited(_)) {
                    error = attempt_error;
                }
            }
            IngestServiceError::RateLimited(_) => error = attempt_error,
            terminal => {
                error = terminal;
                break;
            }
        }
    }
    IngestResponse {
        successes: Vec::new(),
        failures: vec![IngestFailure::Ingesthree {
            subrequest_id: subrequest.subrequest_id,
            index_id: subrequest.index_id.clone(),
            error,
        }],
    }
}

// A malformed remote response is an internal failure, never an empty success.
fn valid_response(subrequest: &IngestSubrequest, response: &IngestResponseV2) -> bool {
    if response.successes.len() + response.failures.len() != 1 {
        return false;
    }
    if let Some(success) = response.successes.first() {
        success.subrequest_id == subrequest.subrequest_id
            && matches!(&success.index_uid, Some(uid) if uid.index_id == subrequest.index_id)
            && success.source_id == subrequest.source_id
            && success.num_ingested_docs as usize + success.parse_failures.len()
                == subrequest
                    .doc_batch
                    .as_ref()
                    .map(|batch| batch.num_docs())
                    .unwrap_or_default()
    } else {
        let failure = &response.failures[0];
        failure.subrequest_id == subrequest.subrequest_id
            && failure.index_id == subrequest.index_id
            && failure.source_id == subrequest.source_id
            && failure.reason() != IngestFailureReason::Unspecified
    }
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
