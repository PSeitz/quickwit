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

//! Dispatch public ingest requests while retaining each backend's error semantics.

use std::sync::Arc;

use quickwit_proto::ingest::IngestV2Error;
use quickwit_proto::ingest::router::{
    self, IngestRequestV2, IngestResponseV2, IngestRouterService, IngestRouterServiceClient,
    IngestSuccess,
};
use quickwit_proto::{ServiceError, ServiceErrorCode};

use crate::IngestServiceError;
use crate::ingesthree::IngesthreeRouter;

#[derive(Clone, Debug)]
pub enum IngestRouterClient {
    V2(IngestRouterServiceClient),
    Ingesthree(Arc<IngesthreeRouter>),
}

impl From<IngestRouterServiceClient> for IngestRouterClient {
    fn from(client: IngestRouterServiceClient) -> Self {
        Self::V2(client)
    }
}

impl From<IngesthreeRouter> for IngestRouterClient {
    fn from(router: IngesthreeRouter) -> Self {
        Self::Ingesthree(Arc::new(router))
    }
}

impl IngestRouterClient {
    pub async fn ingest(
        &self,
        request: IngestRequestV2,
    ) -> Result<IngestResponse, IngestRouterError> {
        match self {
            Self::V2(client) => Ok(client.ingest(request).await?.into()),
            Self::Ingesthree(router) => Ok(router.ingest(request).await?),
        }
    }
}

#[derive(Debug, Default)]
pub struct IngestResponse {
    pub successes: Vec<IngestSuccess>,
    pub failures: Vec<IngestFailure>,
}

impl From<IngestResponseV2> for IngestResponse {
    fn from(response: IngestResponseV2) -> Self {
        Self {
            successes: response.successes,
            failures: response
                .failures
                .into_iter()
                .map(IngestFailure::V2)
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum IngestFailure {
    V2(router::IngestFailure),
    Ingesthree {
        subrequest_id: u32,
        index_id: String,
        error: IngestServiceError,
    },
}

impl IngestFailure {
    pub fn subrequest_id(&self) -> u32 {
        match self {
            Self::V2(failure) => failure.subrequest_id,
            Self::Ingesthree { subrequest_id, .. } => *subrequest_id,
        }
    }

    pub fn index_id(&self) -> &str {
        match self {
            Self::V2(failure) => &failure.index_id,
            Self::Ingesthree { index_id, .. } => index_id,
        }
    }
}

impl From<IngestFailure> for IngestServiceError {
    fn from(failure: IngestFailure) -> Self {
        match failure {
            IngestFailure::V2(failure) => failure.into(),
            IngestFailure::Ingesthree { error, .. } => error,
        }
    }
}

/// Keep V2 request errors intact: ES exposes timeouts as 408, while REST maps them to 503.
#[derive(Debug, thiserror::Error)]
pub enum IngestRouterError {
    #[error(transparent)]
    V2(#[from] IngestV2Error),
    #[error(transparent)]
    Ingesthree(#[from] IngestServiceError),
}

impl ServiceError for IngestRouterError {
    fn error_code(&self) -> ServiceErrorCode {
        match self {
            Self::V2(error) => error.error_code(),
            Self::Ingesthree(error) => error.error_code(),
        }
    }
}

impl From<IngestRouterError> for IngestServiceError {
    fn from(error: IngestRouterError) -> Self {
        match error {
            IngestRouterError::V2(error) => error.into(),
            IngestRouterError::Ingesthree(error) => error,
        }
    }
}

#[cfg(test)]
mod tests {
    use quickwit_proto::ingest::RateLimitingCause;
    use quickwit_proto::ingest::router::IngestFailureReason;

    use super::*;

    #[test]
    fn test_ingesthree_error_mapping_preserves_status_and_message() {
        for (error, code) in [
            (
                IngestServiceError::BadRequest("bad input".into()),
                ServiceErrorCode::BadRequest,
            ),
            (
                IngestServiceError::Unavailable("no destination".into()),
                ServiceErrorCode::Unavailable,
            ),
            (
                IngestServiceError::RateLimited(RateLimitingCause::LoadShedding),
                ServiceErrorCode::TooManyRequests,
            ),
        ] {
            let message = error.to_string();
            let request_error = IngestRouterError::Ingesthree(error.clone());
            assert_eq!(
                request_error.error_code().http_status_code(),
                code.http_status_code()
            );
            assert_eq!(IngestServiceError::from(request_error).to_string(), message);
            let failure = IngestFailure::Ingesthree {
                subrequest_id: 7,
                index_id: "logs".into(),
                error,
            };
            assert_eq!(failure.subrequest_id(), 7);
            assert_eq!(failure.index_id(), "logs");
            let error = IngestServiceError::from(failure);
            assert_eq!(
                error.error_code().http_status_code(),
                code.http_status_code()
            );
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn test_ingesthree_client_preserves_v2_errors_and_partial_results() {
        let error = IngestRouterError::V2(IngestV2Error::Timeout("deadline".into()));
        assert_eq!(error.error_code().http_status_code(), 408);
        assert_eq!(
            IngestServiceError::from(error)
                .error_code()
                .http_status_code(),
            503
        );
        let response = IngestResponse::from(IngestResponseV2 {
            successes: vec![IngestSuccess {
                subrequest_id: 1,
                ..Default::default()
            }],
            failures: vec![router::IngestFailure {
                subrequest_id: 2,
                index_id: "logs".into(),
                reason: IngestFailureReason::IndexNotFound as i32,
                ..Default::default()
            }],
        });
        assert_eq!(response.successes[0].subrequest_id, 1);
        assert_eq!(response.failures[0].subrequest_id(), 2);
        assert_eq!(response.failures[0].index_id(), "logs");
        assert!(
            matches!(IngestServiceError::from(response.failures.into_iter().next().unwrap()),
            IngestServiceError::IndexNotFound { index_id } if index_id == "logs")
        );
    }
}
