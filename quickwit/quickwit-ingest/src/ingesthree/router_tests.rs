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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use quickwit_cluster::{ChitchatTransport, TestClusterBuilder};
use quickwit_config::INGEST_V2_SOURCE_ID;
use quickwit_config::service::QuickwitService;
use quickwit_proto::ingest::DocBatchV2;
use quickwit_proto::ingest::ingesthree::ingesthree_service_server::{
    IngesthreeService, IngesthreeServiceServer,
};
use quickwit_proto::ingest::router::IngestSuccess;
use quickwit_proto::types::{IndexUid, NodeId};
use quickwit_proto::{ServiceError, ServiceErrorCode};
use tokio::net::TcpListener;
use tonic::Response;

use super::*;

#[path = "router_error_tests.rs"]
mod errors;

#[path = "router_metrics_tests.rs"]
mod metrics_tests;

#[path = "router_selection_tests.rs"]
mod selection_tests;

#[derive(Clone)]
struct Intake {
    calls: Arc<AtomicUsize>,
    error: Option<Code>,
    malformed: bool,
}

#[async_trait::async_trait]
impl IngesthreeService for Intake {
    async fn ingest(
        &self,
        request: Request<IngestRequestV2>,
    ) -> Result<Response<IngestResponseV2>, Status> {
        assert_eq!(request.metadata().get(VERSION_HEADER).unwrap(), VERSION);
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = self.error {
            return Err(Status::new(code, "test intake error"));
        }
        if self.malformed {
            return Ok(Response::new(IngestResponseV2::default()));
        }
        let request = request.into_inner();
        assert_eq!(request.subrequests.len(), 1);
        let subrequest = &request.subrequests[0];
        if subrequest.index_id == "pending" {
            return std::future::pending().await;
        }
        if let Some(response) = errors::index_response(subrequest) {
            return response.map(Response::new);
        }
        Ok(Response::new(IngestResponseV2 {
            successes: vec![IngestSuccess {
                subrequest_id: subrequest.subrequest_id,
                index_uid: Some(IndexUid::for_test(&subrequest.index_id, 1)),
                source_id: subrequest.source_id.clone(),
                num_ingested_docs: subrequest.doc_batch.as_ref().unwrap().num_docs() as u32,
                ..Default::default()
            }],
            failures: Vec::new(),
        }))
    }
}

struct Destination {
    cluster: Cluster,
    node: ClusterNode,
    calls: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Destination {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn destination(
    name: &str,
    ingesthree: bool,
    ready: bool,
    zone: &str,
    error: Option<Code>,
    malformed: bool,
) -> Destination {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(AtomicUsize::new(0));
    let intake = Intake {
        calls: calls.clone(),
        error,
        malformed,
    };
    let connections = futures::stream::unfold(listener, |listener| async {
        let accepted = listener.accept().await.map(|(socket, _)| socket);
        Some((accepted, listener))
    });
    let server = tokio::spawn(async {
        tonic::transport::Server::builder()
            .add_service(IngesthreeServiceServer::new(intake))
            .serve_with_incoming(Box::pin(connections))
            .await
            .unwrap();
    });
    let transport = ChitchatTransport::default();
    let cluster = TestClusterBuilder::new(NodeId::from_str(name), port - 1, &transport)
        .with_services(HashSet::from([QuickwitService::Indexer]))
        .build()
        .await
        .unwrap();
    cluster.set_self_ingesthree_enabled(ingesthree).await;
    cluster.set_self_ingesthree_capacity(10).await;
    cluster.set_self_key_value("availability_zone", zone).await;
    cluster.set_self_node_readiness(ready).await;
    let node = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(node) = cluster.live_nodes().await.into_iter().find(|node| {
                node.is_ingesthree() == ingesthree
                    && node.ingesthree_capacity() == Some(10)
                    && node.is_ready == ready
                    && node.availability_zone().as_deref() == Some(zone)
            }) {
                break node;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    Destination {
        cluster,
        node,
        calls,
        server,
    }
}

fn subrequest() -> IngestSubrequest {
    IngestSubrequest {
        subrequest_id: 3,
        index_id: "logs".into(),
        source_id: INGEST_V2_SOURCE_ID.into(),
        doc_batch: Some(DocBatchV2::for_test([r#"{"body":"hello"}"#])),
    }
}

#[tokio::test]
async fn test_ingesthree_destinations_require_capability_readiness_and_prefer_same_az() {
    let remote = destination("remote", true, true, "zone-b", None, false).await;
    let same_zone = destination("same-zone", true, true, "zone-a", None, false).await;
    let v2 = destination("v2", false, true, "zone-a", None, false).await;
    let unready = destination("unready", true, false, "zone-a", None, false).await;
    let nodes = vec![
        remote.node.clone(),
        same_zone.node.clone(),
        v2.node.clone(),
        unready.node.clone(),
    ];
    let zone: AvailabilityZone = "zone-a".into();
    let ordered = destinations(&nodes, "logs", Some(&zone));
    assert_eq!(ordered.len(), 2);
    assert_eq!(ordered[0].node_id, same_zone.node.node_id);
    assert_eq!(ordered[1].node_id, remote.node.node_id);
    // With no local AZ, affinity is stable across discovery ordering and distributes indexes.
    let mut reverse = nodes.clone();
    reverse.reverse();
    let mut winners = HashSet::new();
    for index in 0..100 {
        let index_id = format!("index-{index}");
        let first = destinations(&nodes, &index_id, None)[0].node_id.clone();
        assert_eq!(first, destinations(&reverse, &index_id, None)[0].node_id);
        winners.insert(first);
    }
    assert_eq!(winners.len(), 2);
}

#[tokio::test]
async fn test_ingesthree_router_never_falls_back_to_v2_and_tracks_capability_updates() {
    let node = destination("v2", false, true, "zone-a", None, false).await;
    let router = IngesthreeRouter::new(node.cluster.clone(), None, ByteSize::mb(1));
    let request = IngestRequestV2 {
        subrequests: vec![subrequest()],
        commit_type: CommitTypeV2::Auto as i32,
    };
    let response = router.ingest(request.clone()).await.unwrap();
    assert_eq!(
        IngestServiceError::from(response.failures[0].clone())
            .error_code()
            .http_status_code(),
        503
    );
    assert_eq!(node.calls.load(Ordering::SeqCst), 0);

    let mut changes = node.cluster.change_stream();
    // Consume the initial v2 Add, then observe the gossip capability update.
    assert!(changes.next().await.is_some());
    node.cluster.set_self_ingesthree_enabled(true).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if node
                .cluster
                .ready_nodes()
                .await
                .iter()
                .any(ClusterNode::is_ingesthree)
            {
                break;
            }
            changes.next().await.unwrap();
        }
    })
    .await
    .unwrap();
    let response = router.ingest(request).await.unwrap();
    assert_eq!(response.successes.len(), 1);
    assert_eq!(node.calls.load(Ordering::SeqCst), 1);
}

async fn update_capacity(destination: &Destination, score: Option<usize>) -> ClusterNode {
    match score {
        Some(score) => {
            destination
                .cluster
                .set_self_ingesthree_capacity(score)
                .await
        }
        None => {
            destination
                .cluster
                .remove_self_key("ingesthree_capacity")
                .await
        }
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let node = destination.cluster.ready_nodes().await.pop().unwrap();
            if node.ingesthree_capacity() == score {
                return node;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn test_ingesthree_capacity_filter_preserves_affinity_and_recovers() {
    let local = destination("local", true, true, "zone-a", None, false).await;
    let remote = destination("remote", true, true, "zone-b", None, false).await;
    let zone: AvailabilityZone = "zone-a".into();
    let mut router =
        IngesthreeRouter::new(local.cluster.clone(), Some(zone.clone()), ByteSize::mb(1));
    let metrics = RouterMetrics::for_test("capacity_filter");
    router.metrics = metrics.clone();
    let request = IngestRequestV2 {
        subrequests: vec![subrequest()],
        commit_type: CommitTypeV2::Auto as i32,
    };
    update_capacity(&local, None).await;
    let result = router.ingest(request.clone()).await.unwrap();
    assert_eq!(
        IngestServiceError::from(result.failures[0].clone())
            .error_code()
            .http_status_code(),
        503
    );
    assert_eq!(local.calls.load(Ordering::SeqCst), 0);

    let full = update_capacity(&local, Some(0)).await;
    let nodes = [full, remote.node.clone()];
    let ordered = destinations(&nodes, "logs", Some(&zone));
    assert_eq!(ordered.len(), 2);
    assert_eq!(ordered[0].node_id, local.node.node_id);
    let result = route_subrequest(
        &subrequest(),
        ordered,
        ByteSize::mb(1),
        Some(&zone),
        &metrics,
        None,
    )
    .await;
    assert_eq!(result.successes.len(), 1);
    assert_eq!(local.calls.load(Ordering::SeqCst), 0);
    assert_eq!(remote.calls.load(Ordering::SeqCst), 1);

    // With only a saturated node, report overload without forwarding a payload.
    let result = router.ingest(request.clone()).await.unwrap();
    assert_eq!(
        IngestServiceError::from(result.failures[0].clone())
            .error_code()
            .http_status_code(),
        429
    );
    assert_eq!(local.calls.load(Ordering::SeqCst), 0);

    // Gossip overload evidence survives an unavailable destination's response.
    let down = destination("down", true, true, "zone-b", Some(Code::Unavailable), false).await;
    let result = route_subrequest(
        &subrequest(),
        vec![&nodes[0], &down.node],
        ByteSize::mb(1),
        Some(&zone),
        &metrics,
        None,
    )
    .await;
    assert_eq!(
        IngestServiceError::from(result.failures[0].clone())
            .error_code()
            .http_status_code(),
        429
    );
    assert_eq!(local.calls.load(Ordering::SeqCst), 0);
    assert_eq!(down.calls.load(Ordering::SeqCst), 1);

    // A lower positive score remains eligible and does not override AZ preference.
    let recovered = update_capacity(&local, Some(1)).await;
    let nodes = [recovered, remote.node.clone()];
    assert_eq!(
        destinations(&nodes, "logs", Some(&zone))[0].node_id,
        local.node.node_id
    );
    assert_eq!(router.ingest(request).await.unwrap().successes.len(), 1);
    assert_eq!(local.calls.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.attempt_count("same_az"), 1);
    assert_eq!(metrics.attempt_count("cross_az"), 2);
    assert_eq!(metrics.attempt_count("az_unaware"), 0);
    assert_eq!(metrics.result_count("unavailable"), 1);
    assert_eq!(metrics.result_count("load_shedding"), 1);
    assert_eq!(metrics.result_count("success"), 1);
    assert_eq!(metrics.in_flight_bytes.get(), 0.0);
}

#[tokio::test]
async fn test_ingesthree_retries_capacity_and_unavailable_then_succeeds() {
    let full = destination(
        "full",
        true,
        true,
        "zone-a",
        Some(Code::ResourceExhausted),
        false,
    )
    .await;
    let down = destination("down", true, true, "zone-a", Some(Code::Unavailable), false).await;
    let healthy = destination("healthy", true, true, "zone-b", None, false).await;
    let metrics = RouterMetrics::for_test("retry_success");
    let zone: AvailabilityZone = "zone-a".into();
    let response = route_subrequest(
        &subrequest(),
        vec![&full.node, &down.node, &healthy.node],
        ByteSize::mb(1),
        Some(&zone),
        &metrics,
        None,
    )
    .await;
    assert_eq!(response.successes.len(), 1);
    assert!(response.failures.is_empty());
    for node in [&full, &down, &healthy] {
        assert_eq!(node.calls.load(Ordering::SeqCst), 1);
    }
    metrics.record_response(&response);
    assert_eq!(metrics.attempt_count("same_az"), 2);
    assert_eq!(metrics.attempt_count("cross_az"), 1);
    assert_eq!(metrics.result_count("success"), 1);
    assert_eq!(metrics.result_count("load_shedding"), 0);
    assert_eq!(metrics.result_count("unavailable"), 0);
    let (requests, in_flight) = metrics_tests::client_metrics("retry_success");
    for code in ["resource_exhausted", "unavailable"] {
        assert_eq!(
            quickwit_metrics::counter!(parent: requests, "status" => "error", "code" => code).get(),
            1
        );
    }
    assert_eq!(
        quickwit_metrics::counter!(parent: requests, "status" => "success", "code" => "ok").get(),
        1
    );
    assert_eq!(in_flight.get(), 0.0);
}

#[tokio::test]
async fn test_ingesthree_exhausted_capacity_is_429_compatible_and_retries_are_limited() {
    let full = destination(
        "full",
        true,
        true,
        "zone-a",
        Some(Code::ResourceExhausted),
        false,
    )
    .await;
    let down = destination("down", true, true, "zone-a", Some(Code::Unavailable), false).await;
    let third = destination(
        "third",
        true,
        true,
        "zone-a",
        Some(Code::Unavailable),
        false,
    )
    .await;
    let fourth = destination("fourth", true, true, "zone-a", None, false).await;
    let metrics = RouterMetrics::for_test("retry_limit");
    let response = route_subrequest(
        &subrequest(),
        vec![&full.node, &down.node, &third.node, &fourth.node],
        ByteSize::mb(1),
        None,
        &metrics,
        None,
    )
    .await;
    assert!(response.successes.is_empty());
    assert!(matches!(
        response.failures[0],
        IngestFailure::Ingesthree {
            error: IngestServiceError::RateLimited(RateLimitingCause::LoadShedding),
            ..
        }
    ));
    assert!(matches!(
        crate::IngestServiceError::from(response.failures[0].clone()).error_code(),
        ServiceErrorCode::TooManyRequests
    ));
    assert_eq!(fourth.calls.load(Ordering::SeqCst), 0);
    metrics.record_response(&response);
    assert_eq!(metrics.attempt_count("az_unaware"), 3);
    assert_eq!(metrics.result_count("load_shedding"), 1);
    assert_eq!(metrics.result_count("unavailable"), 0);

    // Advertised saturation skips payloads and leaves the three-attempt budget intact.
    let nodes = [
        update_capacity(&full, Some(0)).await,
        update_capacity(&down, Some(0)).await,
        update_capacity(&third, Some(0)).await,
        fourth.node.clone(),
    ];
    let response = route_subrequest(
        &subrequest(),
        nodes.iter().collect(),
        ByteSize::mb(1),
        None,
        &metrics,
        None,
    )
    .await;
    assert_eq!(response.successes.len(), 1);
    for destination in [&full, &down, &third, &fourth] {
        assert_eq!(destination.calls.load(Ordering::SeqCst), 1);
    }
    assert_eq!(metrics.attempt_count("az_unaware"), 4);
}

#[tokio::test]
async fn test_ingesthree_prefers_own_node_with_positive_capacity() {
    let local = destination("local", true, true, "zone-a", None, false).await;
    let peer = destination("peer", true, true, "zone-a", None, false).await;
    let metrics = RouterMetrics::for_test("local_first");
    for capacity in [Some(1), Some(10), Some(0), None] {
        let local_node = update_capacity(&local, capacity).await;
        let local_before = local.calls.load(Ordering::SeqCst);
        let peer_before = peer.calls.load(Ordering::SeqCst);
        let response = route_subrequest(
            &subrequest(),
            vec![&peer.node, &local_node],
            ByteSize::mb(1),
            None,
            &metrics,
            Some(&local.node.node_id),
        )
        .await;
        assert_eq!(response.successes.len(), 1);
        assert!(response.failures.is_empty());
        let expect_local = usize::from(capacity.is_some_and(|score| score > 0));
        assert_eq!(
            local.calls.load(Ordering::SeqCst),
            local_before + expect_local
        );
        assert_eq!(
            peer.calls.load(Ordering::SeqCst),
            peer_before + 1 - expect_local
        );
    }
}

#[tokio::test]
async fn test_ingesthree_retries_peer_after_own_node_rejects() {
    let local = destination(
        "local",
        true,
        true,
        "zone-a",
        Some(Code::ResourceExhausted),
        false,
    )
    .await;
    let peer = destination("peer", true, true, "zone-a", None, false).await;
    let metrics = RouterMetrics::for_test("local_rejected");
    let response = route_subrequest(
        &subrequest(),
        vec![&peer.node, &local.node],
        ByteSize::mb(1),
        None,
        &metrics,
        Some(&local.node.node_id),
    )
    .await;
    assert_eq!(response.successes.len(), 1);
    assert!(response.failures.is_empty());
    assert_eq!(local.calls.load(Ordering::SeqCst), 1);
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
}
