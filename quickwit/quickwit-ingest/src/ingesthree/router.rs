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
use quickwit_proto::ingest::ingesthree::ingesthree_service_client::IngesthreeServiceClient;
use quickwit_proto::ingest::router::{
    IngestFailureReason, IngestRequestV2, IngestResponseV2, IngestSubrequest,
};
use quickwit_proto::ingest::{CommitTypeV2, RateLimitingCause};
use quickwit_proto::types::AvailabilityZone;
use tonic::{Code, Request, Status};

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
        }
    }

    pub async fn ingest(
        &self,
        request: IngestRequestV2,
    ) -> Result<IngestResponse, IngestServiceError> {
        validate_request(&request)?;
        let ready_nodes = self.cluster.ready_nodes().await;
        let mut response = IngestResponse::default();
        for subrequest in request.subrequests {
            let destinations = destinations(
                &ready_nodes,
                &subrequest.index_id,
                self.availability_zone.as_ref(),
            );
            let result =
                route_subrequest(&subrequest, destinations, self.max_grpc_message_size).await;
            response.successes.extend(result.successes);
            response.failures.extend(result.failures);
        }
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
        // A five-percent score bonus breaks close affinity races in favor of local
        // intake. It never overrides AZ preference or pins every index to this node.
        let local_bonus = if node.is_self_node() {
            u64::MAX as u128 / 20
        } else {
            0
        };
        (
            Reverse(same_zone),
            Reverse(affinity + local_bonus),
            node.node_id.clone(),
        )
    });
    destinations
}

async fn route_subrequest(
    subrequest: &IngestSubrequest,
    mut destinations: Vec<&ClusterNode>,
    max_message_size: ByteSize,
) -> IngestResponse {
    let mut error =
        IngestServiceError::Unavailable("no ready ingest destination accepted the request".into());
    // Capacity gates attempts without changing affinity order or consuming retries.
    // Retain overload evidence just as we do for a destination's rejection below.
    destinations.retain(|node| match node.ingesthree_capacity() {
        Some(0) => {
            error = IngestServiceError::RateLimited(RateLimitingCause::LoadShedding);
            false
        }
        Some(_) => true,
        None => false,
    });
    for node in destinations.into_iter().take(3) {
        let mut client = IngesthreeServiceClient::new(node.channel())
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
        let result = tokio::time::timeout(Duration::from_secs(5), client.ingest(request))
            .await
            .unwrap_or_else(|_| Err(Status::deadline_exceeded("ingesthree intake timed out")));
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
