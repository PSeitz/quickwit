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

use quickwit_metrics::{Counter, Gauge, counter, gauge};

use super::*;

pub(super) fn client_metrics(test: &'static str) -> (Counter, Gauge) {
    let requests = counter!(
        name: "requests_total",
        description: "Total number of gRPC requests processed.",
        subsystem: "grpc",
        "service" => "ingesthree", "grpc_service" => "ingesthree", "kind" => "client", "rpc" => "ingest",
    );
    let in_flight = gauge!(
        name: "requests_in_flight",
        description: "Number of gRPC requests in-flight.",
        subsystem: "grpc",
        "service" => "ingesthree", "grpc_service" => "ingesthree", "kind" => "client", "rpc" => "ingest",
    );
    (
        counter!(parent: requests, "test" => test),
        gauge!(parent: in_flight, "test" => test),
    )
}

#[tokio::test]
async fn test_ingesthree_router_cancellation_releases_memory_and_grpc_in_flight() {
    let node = destination("pending", true, true, "zone-a", None, false).await;
    let metrics = RouterMetrics::for_test("router_cancellation");
    let (requests, in_flight) = client_metrics("router_cancellation");
    let mut router = IngesthreeRouter::new(node.cluster.clone(), None, ByteSize::mb(1));
    router.metrics = metrics.clone();
    let request = IngestRequestV2 {
        subrequests: vec![IngestSubrequest {
            index_id: "pending".into(),
            ..subrequest()
        }],
        commit_type: CommitTypeV2::Auto as i32,
    };
    let num_bytes = request.num_bytes();
    let mut response = Box::pin(router.ingest(request));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            response = &mut response => panic!("pending intake replied: {response:?}"),
            () = async {
                while node.calls.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            } => {}
        }
    })
    .await
    .unwrap();
    assert_eq!(metrics.in_flight_bytes.get(), num_bytes as f64);
    assert_eq!(in_flight.get(), 1.0);
    assert_eq!(metrics.attempt_count("az_unaware"), 1);
    drop(response);
    assert_eq!(metrics.in_flight_bytes.get(), 0.0);
    assert_eq!(in_flight.get(), 0.0);
    assert_eq!(
        counter!(parent: requests, "status" => "cancelled", "code" => "cancelled").get(),
        1
    );
    assert_eq!(metrics.result_count("success"), 0);
    assert_eq!(metrics.result_count("timeout"), 0);
}

#[tokio::test]
async fn test_ingesthree_router_deadline_counts_timeout_and_releases_memory() {
    let node = destination("pending", true, true, "zone-a", None, false).await;
    let metrics = RouterMetrics::for_test("router_deadline");
    let (requests, in_flight) = client_metrics("router_deadline");
    let mut router = IngesthreeRouter::new(node.cluster.clone(), None, ByteSize::mb(1));
    router.metrics = metrics.clone();
    let request = IngestRequestV2 {
        subrequests: vec![IngestSubrequest {
            index_id: "pending".into(),
            ..subrequest()
        }],
        commit_type: CommitTypeV2::Auto as i32,
    };
    let mut response = Box::pin(router.ingest(request));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            response = &mut response => panic!("pending intake replied: {response:?}"),
            () = async {
                while node.calls.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            } => {}
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    let response = response.await.unwrap();
    assert!(
        matches!(&response.failures[0], IngestFailure::V2(failure) if failure.reason() == IngestFailureReason::Timeout),
        "{response:?}"
    );
    assert_eq!(metrics.result_count("timeout"), 1);
    assert_eq!(metrics.result_count("internal"), 0);
    assert_eq!(metrics.attempt_count("az_unaware"), 1);
    assert_eq!(metrics.in_flight_bytes.get(), 0.0);
    assert_eq!(in_flight.get(), 0.0);
    assert_eq!(
        counter!(parent: requests, "status" => "error", "code" => "deadline_exceeded").get(),
        1
    );
    assert_eq!(
        counter!(parent: requests, "status" => "cancelled", "code" => "cancelled").get(),
        0
    );
}
