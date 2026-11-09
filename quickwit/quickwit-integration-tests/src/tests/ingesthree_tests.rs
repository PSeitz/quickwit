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

use std::path::{Path, PathBuf};
use std::time::Duration;

use quickwit_common::test_utils::wait_until_predicate;
use quickwit_config::service::QuickwitService;
use quickwit_config::{ConfigFormat, INGEST_V2_SOURCE_ID};
use quickwit_ingest::ingesthree::DocumentFileReader;
use quickwit_proto::ingest::ingesthree::ingesthree_service_client::IngesthreeServiceClient;
use quickwit_proto::ingest::router::{IngestRequestV2, IngestSubrequest};
use quickwit_proto::ingest::{CommitTypeV2, DocBatchV2};
use quickwit_proto::types::IndexUid;
use quickwit_serve::SearchRequestQueryString;
use reqwest::StatusCode;
use serde_json::Value;

use self::compaction_tests::{assert_compacted, wait_for_splits};
use crate::test_utils::{ClusterSandbox, ClusterSandboxBuilder};

#[path = "ingesthree_compaction_tests.rs"]
mod compaction_tests;

// Node 0 routes public requests and hosts the metastore/searcher, followed by indexers,
// a janitor hosting the planner, and a separate compactor. Real files preserve durable state.
async fn start_cluster(indexer_modes: &[bool]) -> ClusterSandbox {
    quickwit_common::setup_logging_for_tests();
    let mut builder = ClusterSandboxBuilder::default()
        .enable_standalone_compactors()
        .add_node([
            QuickwitService::ControlPlane,
            QuickwitService::Metastore,
            QuickwitService::Searcher,
        ]);
    for _ in indexer_modes {
        builder = builder.add_node([QuickwitService::Indexer]);
    }
    let mut config = builder
        .add_node([QuickwitService::Janitor])
        .add_node([QuickwitService::Compactor])
        .build_config()
        .await;
    // A router without an indexer needs no compaction opt-in.
    config.node_configs[0].0.enable_standalone_compactors = false;
    let root = config.node_configs[0]
        .0
        .data_dir_path
        .parent()
        .unwrap()
        .to_owned();
    for (idx, (node, _)) in config.node_configs.iter_mut().enumerate() {
        node.ingest_api_config.enable_ingesthree =
            idx == 0 || indexer_modes.get(idx - 1) == Some(&true);
        node.metastore_uri = root.join("metastore").to_str().unwrap().parse().unwrap();
        node.default_index_root_uri = root.join("indexes").to_str().unwrap().parse().unwrap();
    }
    config.start().await
}

async fn create_index(sandbox: &ClusterSandbox, index_id: &str) -> IndexUid {
    sandbox
        .rest_client(QuickwitService::Metastore)
        .indexes()
        .create(
            format!(
                r#"
version: 0.8
index_id: {index_id}
doc_mapping:
  field_mappings:
    - name: body
      type: text
indexing_settings:
  commit_timeout_secs: 1
  merge_policy:
    type: limit_merge
    merge_factor: 2
    max_merge_factor: 2
    max_merge_ops: 1
"#
            ),
            ConfigFormat::Yaml,
            false,
        )
        .await
        .unwrap()
        .index_uid
}

async fn post(sandbox: &ClusterSandbox, path: &str, body: &str) -> reqwest::Response {
    let address = sandbox.node_configs[0].0.rest_config.listen_addr;
    reqwest::Client::new()
        .post(format!("http://{address}/api/v1/{path}"))
        .header("content-type", "application/x-ndjson")
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
}

async fn accept(sandbox: &ClusterSandbox, index_id: &str, body: &str) {
    let response = post(sandbox, &format!("{index_id}/ingest"), body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["num_ingested_docs"], 1, "{response}");
    assert_eq!(response["num_rejected_docs"], 0, "{response}");
}

async fn wait_for_hits(sandbox: &ClusterSandbox, index_id: &str, expected: u64) {
    let client = sandbox.rest_client(QuickwitService::Searcher);
    wait_until_predicate(
        || async {
            let result = client
                .search(
                    index_id,
                    SearchRequestQueryString {
                        query: "*".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(
                result.num_hits <= expected,
                "unexpected duplicate documents: {result:?}"
            );
            result.num_hits == expected
        },
        Duration::from_secs(45),
        Duration::from_millis(200),
    )
    .await
    .unwrap();
}

async fn assert_no_shards(sandbox: &ClusterSandbox, index_id: &str) {
    let shards = sandbox
        .rest_client(QuickwitService::Metastore)
        .sources(index_id)
        .get_shards(INGEST_V2_SOURCE_ID)
        .await
        .unwrap();
    assert!(
        shards.is_empty(),
        "ingesthree created v2 shards: {shards:?}"
    );
}

fn input_files(directory: &Path) -> Vec<PathBuf> {
    if !directory.try_exists().unwrap() {
        return Vec::new();
    }
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap() != "LOCK")
        .collect()
}

fn upload_directory(sandbox: &ClusterSandbox) -> PathBuf {
    sandbox.node_configs[0]
        .0
        .default_index_root_uri
        .filepath()
        .unwrap()
        .join(".ingesthree")
}

fn payload(index_id: &str, commit: CommitTypeV2) -> IngestRequestV2 {
    IngestRequestV2 {
        commit_type: commit as i32,
        subrequests: vec![IngestSubrequest {
            subrequest_id: 0,
            index_id: index_id.into(),
            source_id: INGEST_V2_SOURCE_ID.into(),
            doc_batch: Some(DocBatchV2::for_test([r#"{"body":"internal"}"#])),
        }],
    }
}

#[tokio::test]
async fn test_ingesthree_http_and_bulk_compact_through_router_with_v2_member() {
    let sandbox = start_cluster(&[true, false]).await;
    let index_id = "ingesthree-http";
    create_index(&sandbox, index_id).await;
    accept(&sandbox, index_id, r#"{"body":"native"}"#).await;
    // Publication forces the bulk request into a separate file and split for compaction.
    let first = wait_for_splits(&sandbox, index_id, 1, 0).await;
    assert_eq!(first.len(), 1);
    let first_id = first[0].split_metadata.split_id.clone();
    let response = post(
        &sandbox,
        &format!("_elastic/{index_id}/_bulk"),
        "{\"index\":{}}\n{\"body\":\"elastic\"}\n",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["errors"], false, "{response}");
    assert_eq!(response["items"][0]["index"]["status"], 201, "{response}");
    let compactor = &sandbox
        .node_configs
        .iter()
        .find(|(_, services)| services.contains(&QuickwitService::Compactor))
        .unwrap()
        .0;
    let splits = wait_for_splits(&sandbox, index_id, 0, 1).await;
    assert!(assert_compacted(&splits, compactor).contains(&first_id));
    wait_for_hits(&sandbox, index_id, 2).await;
    sandbox.assert_hit_count(index_id, "body:native", 1).await;
    sandbox.assert_hit_count(index_id, "body:elastic", 1).await;
    assert_no_shards(&sandbox, index_id).await;
    assert!(
        !sandbox.node_configs[0]
            .0
            .data_dir_path
            .join("ingesthree")
            .try_exists()
            .unwrap()
    );
    assert!(
        !sandbox.node_configs[2]
            .0
            .data_dir_path
            .join("ingesthree")
            .try_exists()
            .unwrap()
    );
    assert!(!input_files(&upload_directory(&sandbox)).is_empty());
    sandbox.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_ingesthree_guards_reject_before_writes() {
    let sandbox = start_cluster(&[true]).await;
    let index_id = "ingesthree-guards";
    create_index(&sandbox, index_id).await;
    let local = sandbox.node_configs[1].0.data_dir_path.join("ingesthree");
    let endpoint = format!("http://{}", sandbox.node_configs[1].0.grpc_listen_addr);
    let mut v2_client = quickwit_proto::ingest::router::ingest_router_service_grpc_client::IngestRouterServiceGrpcClient::connect(endpoint.clone()).await.unwrap();
    assert_eq!(
        v2_client
            .ingest(payload(index_id, CommitTypeV2::Auto))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unimplemented
    );
    let mut client = IngesthreeServiceClient::connect(endpoint).await.unwrap();
    for version in [None, Some("v2"), Some("INGESTHREE")] {
        let mut request = tonic::Request::new(payload(index_id, CommitTypeV2::Auto));
        if let Some(version) = version {
            request
                .metadata_mut()
                .insert("ingest-version", version.parse().unwrap());
        }
        let status = client.ingest(request).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("ingest-version"));
    }
    let mut duplicate = tonic::Request::new(payload(index_id, CommitTypeV2::Auto));
    for _ in 0..2 {
        duplicate
            .metadata_mut()
            .append("ingest-version", "ingesthree".parse().unwrap());
    }
    assert_eq!(
        client.ingest(duplicate).await.unwrap_err().code(),
        tonic::Code::FailedPrecondition
    );
    for commit in [CommitTypeV2::Force, CommitTypeV2::WaitFor] {
        let mut request = tonic::Request::new(payload(index_id, commit));
        request
            .metadata_mut()
            .insert("ingest-version", "ingesthree".parse().unwrap());
        let status = client.ingest(request).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("Auto"));
    }
    for query in ["commit=force", "commit=wait_for", "use_legacy_ingest=true"] {
        let response = post(
            &sandbox,
            &format!("{index_id}/ingest?{query}"),
            r#"{"body":"rejected"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert!(response.text().await.unwrap().contains("Auto"));
    }
    for refresh in ["true", "wait_for"] {
        let response = post(
            &sandbox,
            &format!("_elastic/{index_id}/_bulk?refresh={refresh}"),
            "{\"index\":{}}\n{\"body\":\"rejected\"}\n",
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.text().await.unwrap().contains("Auto"));
    }
    assert!(
        input_files(&local).is_empty(),
        "rejected requests wrote local input"
    );
    assert!(input_files(&upload_directory(&sandbox)).is_empty());
    assert_no_shards(&sandbox, index_id).await;
    sandbox.assert_hit_count(index_id, "*", 0).await;

    let mut request = tonic::Request::new(payload(index_id, CommitTypeV2::Auto));
    request
        .metadata_mut()
        .insert("ingest-version", "ingesthree".parse().unwrap());
    let response = client.ingest(request).await.unwrap().into_inner();
    assert!(response.failures.is_empty());
    assert_eq!(response.successes.len(), 1);
    assert_eq!(response.successes[0].num_ingested_docs, 1);
    drop(client);
    wait_for_hits(&sandbox, index_id, 1).await;
    sandbox.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_ingesthree_shutdown_uploads_tail_and_restart_indexes_it() {
    let mut sandbox = start_cluster(&[true]).await;
    let index_id = "ingesthree-restart";
    let index_uid = create_index(&sandbox, index_id).await;
    let producer = sandbox.node_configs[1].0.clone();
    let local = producer.data_dir_path.join("ingesthree");
    let uploads = upload_directory(&sandbox);
    let tail = r#"{"body":"tail"}"#;
    accept(&sandbox, index_id, tail).await;
    // Shut down while the accepted document is still in the producer's open file,
    // before its normal ten-second upload timer. Shutdown must finish and upload it.
    let files = input_files(&local);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].extension().unwrap(), "open");
    assert!(input_files(&uploads).is_empty());
    let file_id = files[0].file_stem().unwrap().to_owned();
    let shutdown = sandbox.remove_node(&producer.node_id).unwrap();
    tokio::time::timeout(Duration::from_secs(30), shutdown.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(
        input_files(&local).is_empty(),
        "shutdown retained an unuploaded tail"
    );
    let uploaded = uploads.join(file_id);
    let mut reader = DocumentFileReader::open(
        &uploaded,
        &index_uid,
        std::fs::metadata(&uploaded).unwrap().len(),
    )
    .unwrap();
    assert_eq!(
        reader.read_document().unwrap().unwrap().as_ref(),
        tail.as_bytes()
    );
    assert!(reader.read_document().unwrap().is_none());
    sandbox.assert_hit_count(index_id, "*", 0).await;

    sandbox.restart_node(producer).await;
    // No new request triggers recovery: the restarted worker must discover the registered input.
    wait_for_hits(&sandbox, index_id, 1).await;
    accept(&sandbox, index_id, r#"{"body":"afterrestart"}"#).await;
    wait_for_hits(&sandbox, index_id, 2).await;
    sandbox.assert_hit_count(index_id, "body:tail", 1).await;
    sandbox
        .assert_hit_count(index_id, "body:afterrestart", 1)
        .await;
    assert_no_shards(&sandbox, index_id).await;
    sandbox.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_ingesthree_router_never_falls_back_to_v2() {
    let sandbox = start_cluster(&[false]).await;
    let index_id = "ingesthree-no-fallback";
    create_index(&sandbox, index_id).await;
    let response = post(
        &sandbox,
        &format!("{index_id}/ingest"),
        r#"{"body":"rejected"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let response = post(
        &sandbox,
        &format!("_elastic/{index_id}/_bulk"),
        "{\"index\":{\"_id\":\"unavailable\"}}\n{\"body\":\"rejected\"}\n",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["errors"], true, "{response}");
    assert_eq!(response["items"][0]["index"]["status"], 503, "{response}");
    assert_eq!(
        response["items"][0]["index"]["_id"], "unavailable",
        "{response}"
    );
    let endpoint = format!("http://{}", sandbox.node_configs[1].0.grpc_listen_addr);
    let mut client = IngesthreeServiceClient::connect(endpoint).await.unwrap();
    let mut request = tonic::Request::new(payload(index_id, CommitTypeV2::Auto));
    request
        .metadata_mut()
        .insert("ingest-version", "ingesthree".parse().unwrap());
    assert_eq!(
        client.ingest(request).await.unwrap_err().code(),
        tonic::Code::Unimplemented
    );
    drop(client);
    assert_no_shards(&sandbox, index_id).await;
    assert!(input_files(&upload_directory(&sandbox)).is_empty());
    sandbox.assert_hit_count(index_id, "*", 0).await;
    sandbox.shutdown().await.unwrap();
}
