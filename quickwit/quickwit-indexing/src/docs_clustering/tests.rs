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

use quickwit_common::KillSwitch;
use quickwit_common::io::IoControls;
use quickwit_common::temp_dir::TempDirectory;
use quickwit_config::{ClusteringKey, DocsClusteringConfig};
use quickwit_doc_mapper::{DocMapper, QuickwitDoc, QuickwitDocBatch};
use quickwit_proto::indexing::IndexingPipelineId;
use quickwit_proto::types::{IndexUid, NodeId, PipelineUid};
use tantivy::schema::Value;
use tantivy::{DocAddress, IndexBuilder, TantivyDocument};

use super::{DocClusterer, structure_hash};
use crate::models::IndexedSplitBuilder;

fn mapper() -> DocMapper {
    serde_json::from_value(serde_json::json!({
        "mode": "dynamic",
        "field_mappings": [
            {"name": "body", "type": "text"},
            {"name": "attributes", "type": "object", "field_mappings": [
                {"name": "service", "type": "text"}
            ]},
            {"name": "labels", "type": "json"}
        ]
    }))
    .unwrap()
}

fn map_docs(mapper: &DocMapper, docs: &[serde_json::Value]) -> Vec<QuickwitDoc> {
    let mut batch = QuickwitDocBatch::default();
    for doc in docs {
        batch.push_serialize(doc, 0).unwrap();
    }
    batch.map(mapper).map(|(_, res)| res.unwrap().1).collect()
}

fn builder(
    mapper: &DocMapper,
    group_by: Vec<ClusteringKey>,
) -> anyhow::Result<IndexedSplitBuilder> {
    let config = DocsClusteringConfig { group_by };
    let clusterer = DocClusterer::new(&config)?;
    let index_builder = IndexBuilder::new()
        .schema(mapper.schema())
        .tokenizers(mapper.tokenizer_manager().tantivy_manager().clone());
    IndexedSplitBuilder::new_in_dir(
        IndexingPipelineId {
            index_uid: IndexUid::for_test("clustering", 1),
            source_id: "test-source".into(),
            node_id: NodeId::from_str("test-node"),
            pipeline_uid: PipelineUid::default(),
        },
        0,
        0,
        mapper.doc_mapping_uid(),
        TempDirectory::for_test(),
        index_builder,
        IoControls::default(),
        Some(clusterer),
    )
}

#[test]
fn test_clustering_structure_hash_is_order_and_batch_independent() {
    let mapper = mapper();
    // Different batches assign different dictionary key IDs to the same keys.
    let batch1 = map_docs(
        &mapper,
        &[
            serde_json::json!({"alpha": 1, "beta": {"nested": "x"}}),
            serde_json::json!({"a": {"bc": 1}}),
            serde_json::json!({"ab": {"c": 1}}),
        ],
    );
    let batch2 = map_docs(
        &mapper,
        &[serde_json::json!({"beta": {"nested": [1, 2], "empty": {}}, "alpha": "str"})],
    );
    assert_eq!(structure_hash(&batch1[0]), structure_hash(&batch2[0]));
    assert_ne!(structure_hash(&batch1[1]), structure_hash(&batch1[2]));
}

#[test]
fn test_clustering_field_and_ignored_lexicographic_order() -> anyhow::Result<()> {
    let mapper = mapper();
    let mut split_builder = builder(
        &mapper,
        vec![
            ClusteringKey::Field("service".into()),
            ClusteringKey::Field("labels.status".into()),
        ],
    )?;
    let inputs = [
        serde_json::json!({"body": "b_ok", "service": "b", "labels": {"status": "ok"}}),
        serde_json::json!({"body": "a_ok", "service": "a", "labels": {"status": "ok"}}),
        serde_json::json!({"body": "missing_svc", "labels": {"status": "ok"}}),
        serde_json::json!({"body": "a_err_1", "service": "a", "labels": {"status": "error"}}),
        serde_json::json!({"body": "empty_svc", "service": "", "labels": {"status": "ok"}}),
        serde_json::json!({"body": "a_err_2", "service": "a", "labels": {"status": "error"}}),
        serde_json::json!({"body": "a_missing_st", "service": "a", "labels": {"status": 42}}),
    ];
    for doc in map_docs(&mapper, &inputs) {
        split_builder.add_document(doc)?;
    }
    assert_eq!(
        split_builder
            .doc_clusterer_opt
            .as_ref()
            .unwrap()
            .group_sizes(),
        vec![1, 1, 1, 2, 1, 1]
    );
    let split = split_builder.finalize()?;
    let reader = split.index.reader()?;
    let searcher = reader.searcher();
    let schema = split.index.schema();
    let body = schema.get_field("body")?;
    let mut bodies = Vec::new();
    for doc_id in 0..inputs.len() {
        let doc: TantivyDocument = searcher.doc(DocAddress::new(0, doc_id as u32))?;
        bodies.push(doc.get_first(body).unwrap().as_str().unwrap().to_string());
    }
    // Ignored precedes empty string, then ascending lexicographic order; leaves keep arrival order.
    assert_eq!(
        bodies,
        [
            "missing_svc",
            "empty_svc",
            "a_missing_st",
            "a_err_1",
            "a_err_2",
            "a_ok",
            "b_ok",
        ]
    );
    Ok(())
}

#[test]
fn test_clustering_structure_groups_matching_shapes() -> anyhow::Result<()> {
    let mapper = mapper();
    let mut split_builder = builder(
        &mapper,
        vec![
            ClusteringKey::Field("service".into()),
            ClusteringKey::Structure,
        ],
    )?;
    let inputs = [
        // Shape A: body + _dynamic.service + labels.x
        serde_json::json!({"body": "a1", "service": "api", "labels": {"x": 1}}),
        // Shape B: body + _dynamic.service + labels.x + labels.y
        serde_json::json!({"body": "b1", "service": "api", "labels": {"x": 2, "y": 3}}),
        // Shape A again (different value types, empty object ignored)
        serde_json::json!({"body": "a2", "service": "api", "labels": {"x": "str", "empty": {}}}),
        // Shape B again
        serde_json::json!({"body": "b2", "service": "api", "labels": {"y": [1, 2], "x": null}}),
    ];
    for doc in map_docs(&mapper, &inputs) {
        split_builder.add_document(doc)?;
    }
    assert_eq!(
        split_builder
            .doc_clusterer_opt
            .as_ref()
            .unwrap()
            .group_sizes(),
        vec![2, 2]
    );
    let split = split_builder.finalize()?;
    let reader = split.index.reader()?;
    let searcher = reader.searcher();
    let body = split.index.schema().get_field("body")?;
    let bodies: Vec<String> = (0..4)
        .map(|doc_id| {
            let doc: TantivyDocument = searcher.doc(DocAddress::new(0, doc_id)).unwrap();
            doc.get_first(body).unwrap().as_str().unwrap().to_string()
        })
        .collect();
    // Both shapes are grouped contiguously, preserving arrival order within each shape.
    assert!(
        bodies == ["a1", "a2", "b1", "b2"] || bodies == ["b1", "b2", "a1", "a2"],
        "unexpected order: {bodies:?}"
    );
    Ok(())
}

#[test]
fn test_clustering_mem_usage_counts_shared_batch_once() {
    let mapper = mapper();
    let config = DocsClusteringConfig {
        group_by: vec![ClusteringKey::Field("body".into())],
    };
    let mut clusterer = DocClusterer::new(&config).unwrap();
    let payload = "x".repeat(4096);
    let docs = map_docs(
        &mapper,
        &[
            serde_json::json!({"body": payload}),
            serde_json::json!({"body": "other"}),
            serde_json::json!({"body": "third"}),
        ],
    );
    let (_, batch_mem) = docs[0].batch_data_id_and_mem_usage();
    let before = clusterer.mem_usage();
    clusterer.push(docs[0].clone());
    let after_first = clusterer.mem_usage();
    assert!(after_first - before >= batch_mem);
    clusterer.push(docs[1].clone());
    let after_second = clusterer.mem_usage();
    // Second doc from the same batch must not double-count `BatchData`.
    assert!(after_second - after_first < batch_mem);
}

#[test]
fn test_clustering_tokenized_groups_by_token_pattern_and_max_tokens() -> anyhow::Result<()> {
    let mapper = mapper();
    let mut split_builder = builder(
        &mapper,
        vec![
            ClusteringKey::Field("service".into()),
            ClusteringKey::Tokenized {
                path: "labels.message".into(),
                max_tokens: None,
            },
        ],
    )?;
    let inputs = [
        // Word Gap Number (tag 1,5,2)
        serde_json::json!({"body": "port_1", "service": "api", "labels": {"message": "port 8080"}}),
        // Missing message -> ignored bucket (first within "api")
        serde_json::json!({"body": "no_msg", "service": "api", "labels": {}}),
        // Word Gap IPv4 (tag 1,5,3)
        serde_json::json!({"body": "ip_1", "service": "api", "labels": {"message": "from 10.0.0.1"}}),
        // Non-string message -> ignored bucket
        serde_json::json!({"body": "num_msg", "service": "api", "labels": {"message": 42}}),
        // Same pattern as port_1 (Word Gap Number)
        serde_json::json!({"body": "port_2", "service": "api", "labels": {"message": "started 9090"}}),
        // Same pattern as ip_1 (Word Gap IPv4)
        serde_json::json!({"body": "ip_2", "service": "api", "labels": {"message": "peer 192.168.1.5"}}),
        // Word Gap Uuid (tag 1,5,4)
        serde_json::json!({"body": "uuid_1", "service": "api", "labels": {"message": "id 550e8400-e29b-41d4-a716-446655440000"}}),
    ];
    for doc in map_docs(&mapper, &inputs) {
        split_builder.add_document(doc)?;
    }
    assert_eq!(
        split_builder
            .doc_clusterer_opt
            .as_ref()
            .unwrap()
            .group_sizes(),
        vec![2, 2, 2, 1]
    );
    let split = split_builder.finalize()?;
    let reader = split.index.reader()?;
    let searcher = reader.searcher();
    let body = split.index.schema().get_field("body")?;
    let bodies: Vec<String> = (0..inputs.len() as u32)
        .map(|doc_id| {
            let doc: TantivyDocument = searcher.doc(DocAddress::new(0, doc_id)).unwrap();
            doc.get_first(body).unwrap().as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        bodies,
        [
            "no_msg", "num_msg", "port_1", "port_2", "ip_1", "ip_2", "uuid_1"
        ]
    );

    // `max_tokens: Some(3)` ignores tokens past the 3rd span (Word Gap Number).
    let mut truncated_builder = builder(
        &mapper,
        vec![ClusteringKey::Tokenized {
            path: "labels.message".into(),
            max_tokens: Some(3),
        }],
    )?;
    let truncated_inputs = [
        serde_json::json!({"body": "short", "labels": {"message": "req 10"}}),
        serde_json::json!({"body": "different", "labels": {"message": "10 req"}}),
        serde_json::json!({"body": "long", "labels": {"message": "req 20 extra 10.0.0.1"}}),
    ];
    for doc in map_docs(&mapper, &truncated_inputs) {
        truncated_builder.add_document(doc)?;
    }
    assert_eq!(
        truncated_builder
            .doc_clusterer_opt
            .as_ref()
            .unwrap()
            .group_sizes(),
        vec![2, 1]
    );
    let split = truncated_builder.finalize()?;
    let reader = split.index.reader()?;
    let searcher = reader.searcher();
    let bodies: Vec<String> = (0..truncated_inputs.len() as u32)
        .map(|doc_id| {
            let doc: TantivyDocument = searcher.doc(DocAddress::new(0, doc_id)).unwrap();
            doc.get_first(body).unwrap().as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(bodies, ["short", "long", "different"]);
    Ok(())
}

#[test]
fn test_clustering_finalization_propagates_cancellation() -> anyhow::Result<()> {
    let mapper = mapper();
    let mut split_builder = builder(&mapper, vec![ClusteringKey::Field("body".into())])?;
    for doc in map_docs(&mapper, &[serde_json::json!({"body": "hello"})]) {
        split_builder.add_document(doc)?;
    }
    let kill_switch = KillSwitch::default();
    split_builder
        .controlled_directory
        .set_io_controls(IoControls::default().set_kill_switch(kill_switch.clone()));
    kill_switch.kill();
    assert!(split_builder.finalize().is_err());
    Ok(())
}
