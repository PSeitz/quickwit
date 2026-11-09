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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, FutureExt, Shared};
use quickwit_cluster::{Cluster, ListenerHandle};
use quickwit_common::thread_pool::run_cpu_intensive;
use quickwit_config::{INGEST_V2_SOURCE_ID, SourceInputFormat, SourceParams, build_doc_mapper};
use quickwit_doc_mapper::DocMapper;
use quickwit_metastore::{IndexMetadata, IndexMetadataResponseExt, IndexesMetadataResponseExt};
use quickwit_proto::ServiceError;
use quickwit_proto::ingest::router::IngestFailureReason;
use quickwit_proto::metastore::{
    EntityKind, IndexMetadataRequest, IndexMetadataSubrequest, IndexesMetadataRequest,
    MetastoreError, MetastoreService, MetastoreServiceClient,
};
use quickwit_proto::types::IndexUid;
use tokio::task::JoinHandle;
use tonic::Status;
use tracing::warn;

/// Metastore nodes change this gossip value after index/source mutations. Each change
/// invalidates intake metadata. The value is a notification token, not an ordered revision.
pub const METADATA_CHANGE_KEY: &str = "ingesthree_metadata_change";
const MAX_CACHED_INDEXES: usize = 1_024;
// Like v2's background shard repair, reconciliation runs outside the write path.
// This is a repair cadence, not an expiration or a freshness guarantee.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// An index incarnation, its validated configuration, and its compiled document mapper.
pub(super) struct IntakeIndex {
    pub index_uid: IndexUid,
    pub mapper: Option<Arc<DocMapper>>,
    configuration: serde_json::Value,
}

#[derive(Clone)]
pub(super) enum ResolveError {
    Failure(IngestFailureReason),
    Status(Status),
}

impl From<Status> for ResolveError {
    fn from(status: Status) -> Self {
        Self::Status(status)
    }
}

type Lookup = Shared<BoxFuture<'static, Result<Arc<IntakeIndex>, ResolveError>>>;

/// A shared lookup and the last time an intake request used it.
struct IndexLookup {
    lookup: Arc<Lookup>,
    last_used: Instant,
}

/// Shares initialization and retains successful lookups across service clones.
#[derive(Clone, Default)]
pub(super) struct MetadataCache {
    indexes: Arc<Mutex<HashMap<String, IndexLookup>>>,
}

/// Owns the gossip subscription and background reconciliation for an intake service.
pub(super) struct MetadataWatch {
    _listener: ListenerHandle,
    reconciliation: JoinHandle<()>,
}

impl Drop for MetadataWatch {
    fn drop(&mut self) {
        self.reconciliation.abort();
    }
}

impl MetadataCache {
    pub async fn resolve(
        &self,
        index_id: &str,
        metastore: MetastoreServiceClient,
    ) -> Result<Arc<IntakeIndex>, ResolveError> {
        let lookup = {
            let mut indexes = self.indexes.lock().unwrap();
            if let Some(index) = indexes.get_mut(index_id) {
                index.last_used = Instant::now();
                index.lookup.clone()
            } else {
                if indexes.len() >= MAX_CACHED_INDEXES {
                    let oldest = indexes
                        .iter()
                        .filter(|(_, index)| index.lookup.peek().is_some())
                        .min_by_key(|(_, index)| index.last_used)
                        .map(|(index_id, _)| index_id.clone());
                    let Some(oldest) = oldest else {
                        return Err(Status::resource_exhausted(
                            "too many ingesthree metadata lookups",
                        )
                        .into());
                    };
                    indexes.remove(&oldest);
                }
                let index_id_owned = index_id.to_string();
                // Complete metadata work even if the first caller is cancelled. A
                // suspended shared future must not retain a metastore permit forever.
                let lookup = Arc::new(
                    async move {
                        tokio::time::timeout(
                            Duration::from_secs(30),
                            load_index(&index_id_owned, metastore),
                        )
                        .await
                        .map_err(|_| {
                            ResolveError::Status(Status::unavailable(
                                "index metadata lookup timed out",
                            ))
                        })?
                        .map(Arc::new)
                    }
                    .boxed()
                    .shared(),
                );
                indexes.insert(
                    index_id.to_string(),
                    IndexLookup {
                        lookup: lookup.clone(),
                        last_used: Instant::now(),
                    },
                );
                let completion = lookup.clone();
                let cache = Arc::downgrade(&self.indexes);
                let index_id = index_id.to_string();
                tokio::spawn(async move {
                    // Drive the Shared itself, so completed lookups remain evictable
                    // after caller cancellation and failed lookups are removed.
                    if completion.as_ref().clone().await.is_err()
                        && let Some(cache) = cache.upgrade()
                    {
                        let mut indexes = cache.lock().unwrap();
                        if indexes
                            .get(&index_id)
                            .is_some_and(|index| Arc::ptr_eq(&index.lookup, &completion))
                        {
                            indexes.remove(&index_id);
                        }
                    }
                });
                lookup
            }
        };
        let result = lookup.as_ref().clone().await;
        let mut indexes = self.indexes.lock().unwrap();
        if result.is_err() {
            if indexes
                .get(index_id)
                .is_some_and(|index| Arc::ptr_eq(&index.lookup, &lookup))
            {
                indexes.remove(index_id);
            }
            return result;
        }
        // An invalidated lookup may finish, but it cannot install or return its old
        // configuration. Pointer identity also fences eviction followed by recreation.
        if !indexes
            .get(index_id)
            .is_some_and(|index| Arc::ptr_eq(&index.lookup, &lookup))
        {
            return Err(
                Status::unavailable("index metadata changed during resolution; retry").into(),
            );
        }
        result
    }

    pub fn invalidate(&self) {
        self.indexes.lock().unwrap().clear();
    }

    pub async fn watch(
        &self,
        cluster: &Cluster,
        metastore: MetastoreServiceClient,
    ) -> MetadataWatch {
        let cache = self.clone();
        let listener = cluster
            .subscribe(METADATA_CHANGE_KEY, move |_| cache.invalidate())
            .await;
        let cache = self.clone();
        let reconciliation = tokio::spawn(async move {
            loop {
                tokio::time::sleep(RECONCILE_INTERVAL).await;
                cache.reconcile(metastore.clone()).await;
            }
        });
        MetadataWatch {
            _listener: listener,
            reconciliation,
        }
    }

    async fn reconcile(&self, metastore: MetastoreServiceClient) {
        let lookups: Vec<_> = self
            .indexes
            .lock()
            .unwrap()
            .iter()
            .map(|(index_id, index)| (index_id.clone(), index.lookup.clone()))
            .collect();
        for batch in lookups.chunks(100) {
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                let response = metastore.indexes_metadata(IndexesMetadataRequest {
                    subrequests: batch.iter().map(|(index_id, _)| IndexMetadataSubrequest {
                        index_id: Some(index_id.clone()), index_uid: None,
                    }).collect(),
                }).await.map_err(|error| Status::internal(error.to_string()))?;
                if !response.failures.is_empty() {
                    warn!(failures=?response.failures, "ingesthree metadata reconciliation rejected indexes");
                }
                let metadata = response.deserialize_indexes_metadata().await
                    .map_err(|error| Status::internal(error.to_string()))?;
                let mut configurations = HashMap::new();
                for metadata in metadata {
                    let configuration = configuration(&metadata)?;
                    configurations.insert(metadata.index_uid.index_id, configuration);
                }
                Ok::<_, Status>(configurations)
            }).await;
            let configurations = match result {
                Ok(Ok(configurations)) => Some(configurations),
                error => {
                    // New intake must resolve unverifiable metadata through the metastore.
                    warn!(
                        ?error,
                        "failed to reconcile ingesthree metadata; invalidating batch"
                    );
                    None
                }
            };
            let mut indexes = self.indexes.lock().unwrap();
            for (index_id, lookup) in batch {
                let unchanged = match (
                    lookup.peek(),
                    configurations
                        .as_ref()
                        .and_then(|configs| configs.get(index_id)),
                ) {
                    (Some(Ok(index)), Some(configuration)) => index.configuration == *configuration,
                    _ => false,
                };
                if !unchanged
                    && indexes
                        .get(index_id)
                        .is_some_and(|index| Arc::ptr_eq(&index.lookup, lookup))
                {
                    indexes.remove(index_id);
                }
            }
        }
    }
}

fn configuration(metadata: &IndexMetadata) -> Result<serde_json::Value, Status> {
    // Compare values, not serialized map order. Include all index settings and the
    // built-in source, including mapping UID and eligibility/validation settings.
    serde_json::to_value((
        &metadata.index_uid,
        &metadata.index_config,
        metadata.sources.get(INGEST_V2_SOURCE_ID),
    ))
    .map_err(|error| Status::internal(error.to_string()))
}

async fn load_index(
    index_id: &str,
    metastore: MetastoreServiceClient,
) -> Result<IntakeIndex, ResolveError> {
    let response = match metastore
        .index_metadata(IndexMetadataRequest::for_index_id(index_id.to_string()))
        .await
    {
        Ok(response) => response,
        Err(MetastoreError::NotFound(EntityKind::Index { .. })) => {
            return Err(ResolveError::Failure(IngestFailureReason::IndexNotFound));
        }
        Err(error) => {
            return Err(
                Status::new(error.error_code().grpc_status_code(), error.to_string()).into(),
            );
        }
    };
    let metadata = response
        .deserialize_index_metadata()
        .map_err(|error| Status::internal(error.to_string()))?;
    if metadata.index_uid.index_id != index_id || metadata.index_config.index_id != index_id {
        return Err(Status::internal("metastore returned a different index").into());
    }
    let Some(source) = metadata
        .sources
        .get(INGEST_V2_SOURCE_ID)
        .filter(|source| source.enabled && matches!(source.source_params, SourceParams::Ingest))
    else {
        return Err(ResolveError::Failure(IngestFailureReason::SourceNotFound));
    };
    if source.input_format != SourceInputFormat::Json || source.transform_config.is_some() {
        return Err(Status::invalid_argument(
            "ingesthree requires a JSON ingest source without a transform",
        )
        .into());
    }
    let configuration = configuration(&metadata)?;
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
    Ok(IntakeIndex {
        index_uid: metadata.index_uid,
        mapper,
        configuration,
    })
}
