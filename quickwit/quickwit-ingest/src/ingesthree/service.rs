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

use quickwit_common::thread_pool::run_cpu_intensive;
use quickwit_common::tower::GrpcMetricsLayer;
use quickwit_config::{INGEST_V2_SOURCE_ID, SourceInputFormat, SourceParams, build_doc_mapper};
use quickwit_metastore::IndexMetadataResponseExt;
use quickwit_proto::ServiceError;
use quickwit_proto::ingest::CommitTypeV2;
use quickwit_proto::ingest::ingesthree::ingesthree_service_server::{
    IngesthreeService as GrpcIngesthreeService, IngesthreeServiceServer,
};
use quickwit_proto::ingest::router::{
    IngestFailure, IngestFailureReason, IngestRequestV2, IngestResponseV2, IngestSubrequest,
    IngestSuccess,
};
use quickwit_proto::metastore::{
    EntityKind, IndexMetadataRequest, MetastoreError, MetastoreService, MetastoreServiceClient,
};
use tonic::{Request, Response, Status};
use tower::{Layer, Service};

use super::{IngestError, WriteQueue};
use crate::IngestServiceError;

pub(super) const VERSION_HEADER: &str = "ingest-version";
pub(super) const VERSION: &str = "ingesthree";

/// Resolves each index incarnation and checks its source before file intake.
/// Document validation follows the index's ingest settings.
/// The supplied queue owns recovery, admission limits, upload, and shutdown.
#[derive(Clone)]
pub struct IngesthreeService {
    metastore: MetastoreServiceClient,
    queue: WriteQueue,
    grpc_metrics: GrpcMetricsLayer,
}

impl IngesthreeService {
    pub fn new(metastore: MetastoreServiceClient, queue: WriteQueue) -> Self {
        Self {
            metastore,
            queue,
            grpc_metrics: GrpcMetricsLayer::new("ingesthree", "server"),
        }
    }

    /// Mount separately from the public router and v2 ingester services. The metadata
    /// guard lives in the handler, so every mounting path enforces the same contract.
    pub fn into_grpc_server(self) -> IngesthreeServiceServer<Self> {
        IngesthreeServiceServer::new(self)
    }

    async fn append(&self, subrequest: IngestSubrequest) -> Result<IngestResponseV2, Status> {
        let reject = |reason| {
            Ok(IngestResponseV2 {
                successes: Vec::new(),
                failures: vec![failure(&subrequest, reason)],
            })
        };
        if subrequest.source_id != INGEST_V2_SOURCE_ID {
            return reject(IngestFailureReason::SourceNotFound);
        }
        let metadata_response = match self
            .metastore
            .index_metadata(IndexMetadataRequest::for_index_id(
                subrequest.index_id.clone(),
            ))
            .await
        {
            Ok(response) => response,
            Err(MetastoreError::NotFound(EntityKind::Index { .. })) => {
                return reject(IngestFailureReason::IndexNotFound);
            }
            Err(error) => {
                return Err(Status::new(
                    error.error_code().grpc_status_code(),
                    error.to_string(),
                ));
            }
        };
        let metadata = metadata_response
            .deserialize_index_metadata()
            .map_err(|error| Status::internal(error.to_string()))?;
        if metadata.index_uid.index_id != subrequest.index_id
            || metadata.index_config.index_id != subrequest.index_id
        {
            return Err(Status::internal("metastore returned a different index"));
        }
        let Some(source) = metadata.sources.get(INGEST_V2_SOURCE_ID).filter(|source| {
            source.enabled && matches!(source.source_params, SourceParams::Ingest)
        }) else {
            return reject(IngestFailureReason::SourceNotFound);
        };
        // S3Indexer consumes JSON directly and carries a single source label.
        // Applying a source transform again during indexing would change accepted data.
        if source.input_format != SourceInputFormat::Json || source.transform_config.is_some() {
            return Err(Status::invalid_argument(
                "ingesthree requires a JSON ingest source without a transform",
            ));
        }
        let index_config = metadata.index_config;
        let mapper = if index_config.ingest_settings.validate_docs {
            Some(
                run_cpu_intensive(move || {
                    build_doc_mapper(&index_config.doc_mapping, &index_config.search_settings)
                })
                .await
                .map_err(|error| Status::internal(error.to_string()))?
                .map_err(|error| Status::internal(error.to_string()))?,
            )
        } else {
            None
        };
        let batch = subrequest
            .doc_batch
            .expect("request shape checked before append");
        let appended = self
            .queue
            .append(metadata.index_uid.clone(), batch, mapper)
            .await
            .map_err(|error| match error {
                IngestError::QueueFull | IngestError::StorageFull | IngestError::OpenFileLimit => {
                    Status::resource_exhausted(error.to_string())
                }
                IngestError::Unavailable => Status::unavailable(error.to_string()),
                IngestError::InvalidRequest(message) => Status::invalid_argument(message),
                IngestError::Internal(error) => Status::internal(error.to_string()),
            })?;
        Ok(IngestResponseV2 {
            successes: vec![IngestSuccess {
                subrequest_id: subrequest.subrequest_id,
                index_uid: Some(metadata.index_uid),
                source_id: subrequest.source_id,
                num_ingested_docs: appended.num_docs as u32,
                parse_failures: appended.parse_failures,
                // File appends have no shard or replication position.
                ..Default::default()
            }],
            failures: Vec::new(),
        })
    }

    async fn ingest_request(
        &self,
        request: Request<IngestRequestV2>,
    ) -> Result<Response<IngestResponseV2>, Status> {
        // Check before consuming the payload, resolving indexes, or admitting writes.
        let mut versions = request.metadata().get_all(VERSION_HEADER).iter();
        if versions.next().map(|value| value.as_bytes()) != Some(VERSION.as_bytes())
            || versions.next().is_some()
        {
            return Err(Status::failed_precondition(
                "required metadata: ingest-version: ingesthree",
            ));
        }
        let request = request.into_inner();
        validate_request(&request).map_err(|error| Status::invalid_argument(error.to_string()))?;
        // The router splits a public request into individual index batches. A status
        // therefore never hides successful writes to another index in the same RPC.
        if request.subrequests.len() != 1 {
            return Err(Status::invalid_argument(
                "internal intake requires exactly one subrequest",
            ));
        }
        let subrequest = request.subrequests.into_iter().next().unwrap();
        self.append(subrequest).await.map(Response::new)
    }
}

#[async_trait::async_trait]
impl GrpcIngesthreeService for IngesthreeService {
    async fn ingest(
        &self,
        request: Request<IngestRequestV2>,
    ) -> Result<Response<IngestResponseV2>, Status> {
        self.grpc_metrics
            .layer(tower::service_fn(|request| self.ingest_request(request)))
            .call(request)
            .await
    }
}

pub(super) fn failure(subrequest: &IngestSubrequest, reason: IngestFailureReason) -> IngestFailure {
    IngestFailure {
        subrequest_id: subrequest.subrequest_id,
        index_id: subrequest.index_id.clone(),
        source_id: subrequest.source_id.clone(),
        reason: reason as i32,
    }
}

/// Reject request-wide errors before any index can accept data.
pub(super) fn validate_request(request: &IngestRequestV2) -> Result<(), IngestServiceError> {
    if request.commit_type != CommitTypeV2::Auto as i32 {
        return Err(IngestServiceError::BadRequest(
            "ingesthree supports only Auto commits; Force and WaitFor are unsupported".into(),
        ));
    }
    let mut subrequest_ids = HashSet::new();
    for subrequest in &request.subrequests {
        if !subrequest_ids.insert(subrequest.subrequest_id) {
            return Err(IngestServiceError::BadRequest(
                "duplicate subrequest ID".into(),
            ));
        }
        let Some(batch) = &subrequest.doc_batch else {
            return Err(IngestServiceError::BadRequest(
                "missing document batch".into(),
            ));
        };
        let document_bytes = batch
            .doc_lengths
            .iter()
            .try_fold(0usize, |sum, len| sum.checked_add(*len as usize));
        if batch.is_empty()
            || batch.doc_lengths.len() != batch.doc_uids.len()
            || document_bytes != Some(batch.doc_buffer.len())
            || batch.doc_uids.iter().collect::<HashSet<_>>().len() != batch.doc_uids.len()
        {
            return Err(IngestServiceError::BadRequest(
                "empty or inconsistent document batch".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use quickwit_config::{IndexConfig, SourceConfig};
    use quickwit_metastore::IndexMetadata;
    use quickwit_metrics::{Counter, Gauge, counter, gauge, labels};
    use quickwit_proto::ServiceErrorCode;
    use quickwit_proto::ingest::DocBatchV2;
    use quickwit_proto::metastore::{IndexMetadataResponse, MockMetastoreService};
    use quickwit_proto::types::IndexUid;

    use super::*;
    use crate::ingesthree::DocumentFileReader;

    fn payload() -> IngestRequestV2 {
        IngestRequestV2 {
            commit_type: CommitTypeV2::Auto as i32,
            subrequests: vec![IngestSubrequest {
                subrequest_id: 7,
                index_id: "logs".into(),
                source_id: INGEST_V2_SOURCE_ID.into(),
                doc_batch: Some(DocBatchV2::for_test([r#"{"body":"hello"}"#])),
            }],
        }
    }

    fn request(payload: IngestRequestV2) -> Request<IngestRequestV2> {
        let mut request = Request::new(payload);
        request
            .metadata_mut()
            .insert(VERSION_HEADER, VERSION.parse().unwrap());
        request
    }

    fn metadata() -> IndexMetadata {
        let mut config = IndexConfig::for_test("logs", "ram:///logs");
        config.doc_mapping.timestamp_field = None;
        let mut metadata =
            IndexMetadata::new_with_index_uid(IndexUid::for_test("logs", 123), config);
        metadata
            .add_source(SourceConfig::for_test(
                INGEST_V2_SOURCE_ID,
                SourceParams::Ingest,
            ))
            .unwrap();
        metadata
    }

    fn metastore(metadata: IndexMetadata, calls: usize) -> MetastoreServiceClient {
        let mut mock = MockMetastoreService::new();
        mock.expect_index_metadata()
            .times(calls)
            .returning(move |request| {
                assert_eq!(request, IndexMetadataRequest::for_index_id("logs".into()));
                IndexMetadataResponse::try_from_index_metadata(&metadata)
            });
        MetastoreServiceClient::from_mock(mock)
    }

    fn service_metrics(service: &mut IngesthreeService, test: &'static str) -> (Counter, Gauge) {
        service.grpc_metrics =
            GrpcMetricsLayer::new_with_labels("ingesthree", "server", labels!("test" => test));
        let requests = counter!(
            name: "requests_total",
            description: "Total number of gRPC requests processed.",
            subsystem: "grpc",
            "service" => "ingesthree", "grpc_service" => "ingesthree", "kind" => "server", "rpc" => "ingest",
        );
        let in_flight = gauge!(
            name: "requests_in_flight",
            description: "Number of gRPC requests in-flight.",
            subsystem: "grpc",
            "service" => "ingesthree", "grpc_service" => "ingesthree", "kind" => "server", "rpc" => "ingest",
        );
        (
            counter!(parent: requests, "test" => test),
            gauge!(parent: in_flight, "test" => test),
        )
    }

    #[tokio::test]
    async fn test_ingesthree_service_cancellation_releases_grpc_in_flight() {
        let directory = tempfile::tempdir().unwrap();
        let queue = WriteQueue::open(directory.path().into(), 100_000, 100_000, None)
            .await
            .unwrap();
        let metastore = MetastoreServiceClient::tower()
            .stack_index_metadata_layer(tower::layer::layer_fn(|_inner| {
                tower::service_fn(|_: IndexMetadataRequest| {
                    std::future::pending::<Result<IndexMetadataResponse, MetastoreError>>()
                })
            }))
            .build_from_mock(MockMetastoreService::new());
        let mut service = IngesthreeService::new(metastore, queue.clone());
        let (requests, in_flight) = service_metrics(&mut service, "service_cancellation");
        let mut response = Box::pin(service.ingest(request(payload())));
        assert!(futures::poll!(&mut response).is_pending());
        assert_eq!(in_flight.get(), 1.0);
        drop(response);
        assert_eq!(in_flight.get(), 0.0);
        assert_eq!(
            counter!(parent: requests, "status" => "cancelled", "code" => "cancelled").get(),
            1
        );
        assert!(queue.finish().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ingesthree_metadata_guard_precedes_lookup_and_writes() {
        let directory = tempfile::tempdir().unwrap();
        let queue = WriteQueue::open(directory.path().into(), 100_000, 100_000, None)
            .await
            .unwrap();
        // Any unexpected metastore lookup fails this mock.
        let mut service = IngesthreeService::new(
            MetastoreServiceClient::from_mock(MockMetastoreService::new()),
            queue.clone(),
        );
        let (requests, in_flight) = service_metrics(&mut service, "metadata_guard");
        for version in [None, Some("v2"), Some("INGESTHREE"), Some("ingesthree ")] {
            let mut request = Request::new(payload());
            if let Some(version) = version {
                request
                    .metadata_mut()
                    .insert(VERSION_HEADER, version.parse().unwrap());
            }
            let status = service.ingest(request).await.unwrap_err();
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        }
        let mut duplicate = request(payload());
        duplicate
            .metadata_mut()
            .append(VERSION_HEADER, VERSION.parse().unwrap());
        assert_eq!(
            service.ingest(duplicate).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            counter!(parent: requests, "status" => "error", "code" => "failed_precondition").get(),
            5
        );
        assert_eq!(in_flight.get(), 0.0);
        assert!(queue.finish().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ingesthree_rejects_commit_modes_and_malformed_batches_before_writes() {
        let directory = tempfile::tempdir().unwrap();
        let queue = WriteQueue::open(directory.path().into(), 100_000, 100_000, None)
            .await
            .unwrap();
        let service = IngesthreeService::new(
            MetastoreServiceClient::from_mock(MockMetastoreService::new()),
            queue.clone(),
        );
        for commit_type in [
            CommitTypeV2::Force as i32,
            CommitTypeV2::WaitFor as i32,
            0,
            99,
        ] {
            let mut payload = payload();
            payload.commit_type = commit_type;
            assert_eq!(
                service.ingest(request(payload)).await.unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
        let mut malformed = payload();
        malformed.subrequests[0]
            .doc_batch
            .as_mut()
            .unwrap()
            .doc_lengths[0] = 999;
        assert_eq!(
            service.ingest(request(malformed)).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut multiple = payload();
        let mut second = multiple.subrequests[0].clone();
        second.subrequest_id += 1;
        multiple.subrequests.push(second);
        assert_eq!(
            service.ingest(request(multiple)).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert!(queue.finish().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ingesthree_resolves_incarnation_and_applies_validation_setting() {
        let valid = r#"{"body":"hello"}"#;
        let invalid = "invalid JSON";
        for (validate_docs, documents, expected_document) in [
            (true, vec![valid, invalid], valid),
            (false, vec![invalid], invalid),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let queue = WriteQueue::open(directory.path().into(), 100_000, 100_000, None)
                .await
                .unwrap();
            let mut metadata = metadata();
            metadata.index_config.ingest_settings.validate_docs = validate_docs;
            let mut service = IngesthreeService::new(metastore(metadata, 1), queue.clone());
            let (requests, in_flight) = service_metrics(
                &mut service,
                if validate_docs {
                    "validation_enabled"
                } else {
                    "validation_disabled"
                },
            );
            let mut payload = payload();
            payload.subrequests[0].doc_batch = Some(DocBatchV2::for_test(documents));
            let response = service.ingest(request(payload)).await.unwrap().into_inner();
            assert!(response.failures.is_empty());
            assert_eq!(response.successes.len(), 1);
            let success = &response.successes[0];
            assert_eq!(success.index_uid, Some(IndexUid::for_test("logs", 123)));
            assert_eq!(success.source_id, INGEST_V2_SOURCE_ID);
            assert_eq!(success.num_ingested_docs, 1);
            assert_eq!(success.parse_failures.len(), usize::from(validate_docs));
            assert!(success.shard_id.is_none());
            assert!(success.replication_position_inclusive.is_none());
            assert_eq!(
                counter!(parent: requests, "status" => "success", "code" => "ok").get(),
                1
            );
            assert_eq!(in_flight.get(), 0.0);
            let files = queue.finish().await.unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].index_uid, IndexUid::for_test("logs", 123));
            assert_eq!(files[0].num_docs, 1);
            let mut reader =
                DocumentFileReader::open(&files[0].path, &files[0].index_uid, files[0].num_bytes)
                    .unwrap();
            assert_eq!(reader.read_document().unwrap().unwrap(), expected_document);
            assert!(reader.read_document().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn test_ingesthree_source_checks_and_missing_index_do_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let queue = WriteQueue::open(directory.path().into(), 100_000, 100_000, None)
            .await
            .unwrap();
        let service = IngesthreeService::new(
            MetastoreServiceClient::from_mock(MockMetastoreService::new()),
            queue.clone(),
        );
        let mut wrong_source = payload();
        wrong_source.subrequests[0].source_id = "other".into();
        let response = service
            .ingest(request(wrong_source))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response.failures[0].reason(),
            IngestFailureReason::SourceNotFound
        );

        let mut disabled = metadata();
        disabled
            .sources
            .get_mut(INGEST_V2_SOURCE_ID)
            .unwrap()
            .enabled = false;
        let service = IngesthreeService::new(metastore(disabled, 1), queue.clone());
        let response = service
            .ingest(request(payload()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response.failures[0].reason(),
            IngestFailureReason::SourceNotFound
        );

        let mut missing = MockMetastoreService::new();
        missing.expect_index_metadata().times(1).returning(|_| {
            Err(MetastoreError::NotFound(EntityKind::Index {
                index_id: "logs".into(),
            }))
        });
        let service =
            IngesthreeService::new(MetastoreServiceClient::from_mock(missing), queue.clone());
        let response = service
            .ingest(request(payload()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response.failures[0].reason(),
            IngestFailureReason::IndexNotFound
        );

        let mut non_json = metadata();
        non_json
            .sources
            .get_mut(INGEST_V2_SOURCE_ID)
            .unwrap()
            .input_format = SourceInputFormat::PlainText;
        let service = IngesthreeService::new(metastore(non_json, 1), queue.clone());
        assert_eq!(
            service.ingest(request(payload())).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert!(queue.finish().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ingesthree_capacity_and_draining_are_honest_errors() {
        for (queue_bytes, disk_bytes) in [(1, 100_000), (100_000, 1)] {
            let directory = tempfile::tempdir().unwrap();
            let queue = WriteQueue::open(directory.path().into(), queue_bytes, disk_bytes, None)
                .await
                .unwrap();
            let service = IngesthreeService::new(metastore(metadata(), 2), queue.clone());
            assert_eq!(
                service.ingest(request(payload())).await.unwrap_err().code(),
                tonic::Code::ResourceExhausted
            );
            assert!(queue.finish().await.unwrap().is_empty());
            assert_eq!(
                service.ingest(request(payload())).await.unwrap_err().code(),
                tonic::Code::Unavailable
            );
        }
        let mut invalid = payload();
        invalid.commit_type = CommitTypeV2::Force as i32;
        assert!(matches!(
            validate_request(&invalid).unwrap_err().error_code(),
            ServiceErrorCode::BadRequest
        ));
    }
}
