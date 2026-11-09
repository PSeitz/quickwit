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

use super::*;
use crate::ingesthree::service::failure;
use crate::router_client::{IngestRouterClient, IngestRouterError};

// The fixture returns index-specific failures to exercise mixed public requests.
pub(super) fn index_response(
    subrequest: &IngestSubrequest,
) -> Option<Result<IngestResponseV2, Status>> {
    let reason = match subrequest.index_id.as_str() {
        "bad-input" => {
            return Some(Err(Status::invalid_argument(
                "unsupported source configuration",
            )));
        }
        "unavailable" => return Some(Err(Status::unavailable("intake draining"))),
        "overloaded" => return Some(Err(Status::resource_exhausted("queue full"))),
        "missing-index" => IngestFailureReason::IndexNotFound,
        "missing-source" => IngestFailureReason::SourceNotFound,
        _ => return None,
    };
    Some(Ok(IngestResponseV2 {
        successes: Vec::new(),
        failures: vec![failure(subrequest, reason)],
    }))
}

#[tokio::test]
async fn test_ingesthree_terminal_errors_and_malformed_responses_are_not_retried() {
    let healthy = destination("healthy", true, true, "zone-a", None, false).await;
    for (error, malformed, expected) in [
        (Some(Code::Internal), false, 500),
        (Some(Code::DeadlineExceeded), false, 500),
        (Some(Code::InvalidArgument), false, 400),
        (None, true, 500),
    ] {
        let bad = destination("bad", true, true, "zone-a", error, malformed).await;
        let response = route_subrequest(
            &subrequest(),
            vec![&bad.node, &healthy.node],
            ByteSize::mb(1),
        )
        .await;
        assert_eq!(
            IngestServiceError::from(response.failures[0].clone())
                .error_code()
                .http_status_code(),
            expected
        );
    }
    assert_eq!(healthy.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_ingesthree_client_preserves_partial_results_and_backend_errors() {
    let node = destination("intake", true, true, "zone-a", None, false).await;
    let client = IngestRouterClient::from(IngesthreeRouter::new(
        node.cluster.clone(),
        None,
        ByteSize::mb(1),
    ));
    let indexes = [
        "logs",
        "bad-input",
        "unavailable",
        "overloaded",
        "missing-index",
        "missing-source",
    ];
    let request = IngestRequestV2 {
        commit_type: CommitTypeV2::Auto as i32,
        subrequests: indexes
            .iter()
            .enumerate()
            .map(|(index, index_id)| IngestSubrequest {
                subrequest_id: index as u32,
                index_id: (*index_id).into(),
                ..subrequest()
            })
            .collect(),
    };
    let response = client.ingest(request).await.unwrap();
    assert_eq!(response.successes.len(), 1);
    assert_eq!(response.successes[0].subrequest_id, 0);
    assert_eq!(response.failures.len(), 5);
    for (index, (failure, status)) in response
        .failures
        .into_iter()
        .zip([400, 503, 429, 404, 500])
        .enumerate()
    {
        assert_eq!(failure.subrequest_id(), index as u32 + 1);
        assert_eq!(failure.index_id(), indexes[index + 1]);
        if index < 3 {
            assert!(matches!(failure, IngestFailure::Ingesthree { .. }));
        } else {
            assert!(matches!(failure, IngestFailure::V2(_)));
        }
        assert_eq!(
            IngestServiceError::from(failure)
                .error_code()
                .http_status_code(),
            status
        );
    }
    assert_eq!(node.calls.load(Ordering::SeqCst), indexes.len());
}

#[tokio::test]
async fn test_ingesthree_client_rejects_request_errors_before_any_write() {
    let node = destination("intake", true, true, "zone-a", None, false).await;
    let client = IngestRouterClient::from(IngesthreeRouter::new(
        node.cluster.clone(),
        None,
        ByteSize::mb(1),
    ));
    let valid = IngestRequestV2 {
        commit_type: CommitTypeV2::Auto as i32,
        subrequests: vec![subrequest()],
    };
    let mut force = valid.clone();
    force.commit_type = CommitTypeV2::Force as i32;
    let mut duplicate = valid.clone();
    duplicate.subrequests.push(subrequest());
    let mut malformed = valid;
    let mut bad_batch = subrequest();
    bad_batch.subrequest_id += 1;
    bad_batch.doc_batch = None;
    malformed.subrequests.push(bad_batch);
    for request in [force, duplicate, malformed] {
        let error = client.ingest(request).await.unwrap_err();
        assert!(matches!(
            error,
            IngestRouterError::Ingesthree(IngestServiceError::BadRequest(_))
        ));
        assert_eq!(error.error_code().http_status_code(), 400);
    }
    assert_eq!(node.calls.load(Ordering::SeqCst), 0);
}
