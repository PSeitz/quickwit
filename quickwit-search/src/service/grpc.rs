/*
 * Copyright (C) 2021 Quickwit Inc.
 *
 * Quickwit is offered under the AGPL v3.0 and as commercial software.
 * For commercial licensing, contact us at hello@quickwit.io.
 *
 * AGPL:
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */

use std::sync::Arc;

use async_trait::async_trait;
<<<<<<< HEAD:quickwit-serve/src/grpc_adapter.rs

use quickwit_proto::search_service_server as grpc;
use quickwit_search::{SearchError, SearchService, SearchServiceImpl};

/// gRPC adapter that wraped SearchService.
=======

use quickwit_proto::search_service_server::SearchService as ProtoSearchService;
use quickwit_proto::{
    FetchDocsRequest, FetchDocsResult, LeafSearchRequest, LeafSearchResult, SearchRequest,
    SearchResult,
};

use crate::service::rest::{SearchService, SearchServiceImpl};
use crate::SearchError;

/// gRPC search service that wraped SearchService.
>>>>>>> Add gRPC server.:quickwit-search/src/service/grpc.rs
#[derive(Clone)]
pub struct GrpcSearchServiceImpl(Arc<dyn SearchService>);

impl From<Arc<SearchServiceImpl>> for GrpcSearchServiceImpl {
    fn from(search_service_arc: Arc<SearchServiceImpl>) -> Self {
        GrpcSearchServiceImpl(search_service_arc)
    }
}

#[async_trait]
impl ProtoSearchService for GrpcSearchServiceImpl {
    async fn root_search(
        &self,
        request: tonic::Request<quickwit_proto::SearchRequest>,
    ) -> Result<tonic::Response<quickwit_proto::SearchResult>, tonic::Status> {
        let search_request = request.into_inner();
        let search_result = self
            .0
            .root_search(search_request)
            .await
            .map_err(convert_error_to_tonic_status)?;
        Ok(tonic::Response::new(search_result))
    }

    async fn leaf_search(
        &self,
        request: tonic::Request<quickwit_proto::LeafSearchRequest>,
    ) -> Result<tonic::Response<quickwit_proto::LeafSearchResult>, tonic::Status> {
        let leaf_search_request = request.into_inner();
        let leaf_search_result = self
            .0
            .leaf_search(leaf_search_request)
            .await
            .map_err(convert_error_to_tonic_status)?;
        Ok(tonic::Response::new(leaf_search_result))
    }

    async fn fetch_docs(
        &self,
        request: tonic::Request<quickwit_proto::FetchDocsRequest>,
    ) -> Result<tonic::Response<quickwit_proto::FetchDocsResult>, tonic::Status> {
        let fetch_docs_request = request.into_inner();
        let fetch_docs_result = self
            .0
            .fetch_docs(fetch_docs_request)
            .await
            .map_err(convert_error_to_tonic_status)?;
        Ok(tonic::Response::new(fetch_docs_result))
    }
}

/// Convert quickwit search error to tonic status.
fn convert_error_to_tonic_status(search_error: SearchError) -> tonic::Status {
    match search_error {
        SearchError::IndexDoesNotExist { index_id } => tonic::Status::new(
            tonic::Code::NotFound,
            format!("Index not found {}", index_id),
        ),
        SearchError::InternalError(error) => tonic::Status::new(
            tonic::Code::Internal,
            format!("Internal error: {:?}", error),
        ),
        SearchError::StorageResolverError(storage_resolver_error) => tonic::Status::new(
            tonic::Code::Internal,
            format!("Failed to resolve storage uri {:?}", storage_resolver_error),
        ),
        SearchError::InvalidQuery(query_error) => tonic::Status::new(
            tonic::Code::InvalidArgument,
            format!("Invalid query: {:?}", query_error),
        ),
    }
}
