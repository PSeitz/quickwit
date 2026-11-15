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

//! Tantivy retains documents in each split writer and indexes them in cluster order.
//!
//! `DocsClusteringConfig::fields` is an ordered list of mapped schema/JSON paths.
//! Mapped object leaves use their schema names, such as `attributes.service`.
//! Unmapped dynamic values require explicit paths such as `_dynamic.service`.
//! Unknown schema fields and traversal through scalar fields fail writer creation.
//! There is no fallback to `_source`, `_dynamic`, or another field. JSON paths use
//! Tantivy's dot escaping. They traverse retained object keys, so a literal dotted
//! JSON key needs an escaped dot even when its indexing options expand dots.
//!
//! Strings define groups. Repeated fields and arrays use the first value. Missing
//! and non-string values share an ignored bucket. Larger groups come first at
//! each level. Equal-size groups follow first arrival, and leaf groups retain
//! document arrival order. Document contents remain unchanged.
//!
//! Clustering is optional. Node configuration applies `QW_DISABLE_DOCS_CLUSTERING`.
//! Writers receive clustering before any documents and use ordinary finalization.
//! Memory-based flushes count retained document capacities and grouping allocations.
//! Finalization also needs memory for growing indexing structures and output buffers.
//! Its peak is outside this retained-memory estimate. Indexing errors propagate at
//! finalization. Ingesthree finishes every split before starting uploads, so any such
//! error fails the whole assignment.

#[cfg(test)]
mod tests {
    use quickwit_common::KillSwitch;
    use quickwit_common::io::IoControls;
    use quickwit_common::temp_dir::TempDirectory;
    use quickwit_config::{DocsClusteringConfig, IndexingSettings};
    use quickwit_doc_mapper::DocMapper;
    use quickwit_proto::indexing::IndexingPipelineId;
    use quickwit_proto::types::{IndexUid, NodeId, PipelineUid};
    use tantivy::schema::Value;
    use tantivy::{DocAddress, Document, TantivyDocument, doc};

    use crate::models::{IndexedSplitBuilder, ProcessedDoc};

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

    fn builder(mapper: &DocMapper, fields: &[&str]) -> anyhow::Result<IndexedSplitBuilder> {
        IndexedSplitBuilder::from_doc_mapper(
            IndexingPipelineId {
                index_uid: IndexUid::for_test("clustering", 1),
                source_id: "test-source".into(),
                node_id: NodeId::from_str("test-node"),
                pipeline_uid: PipelineUid::default(),
            },
            0,
            0,
            mapper,
            &IndexingSettings::for_test(),
            TempDirectory::for_test(),
            IoControls::default(),
            Some(&DocsClusteringConfig {
                fields: fields.iter().map(|field| field.to_string()).collect(),
            }),
        )
    }

    #[test]
    fn test_clustering_schema_paths() {
        let mapper = mapper();
        for path in [
            "body",
            "attributes.service",
            "labels.service",
            "_dynamic.service",
        ] {
            assert!(builder(&mapper, &[path]).is_ok(), "{path}");
        }
        // An unmapped source field must name its dynamic schema container explicitly.
        for path in ["service", "body.service", "attributes"] {
            assert!(builder(&mapper, &[path]).is_err(), "{path}");
        }
    }

    #[test]
    fn test_clustering_finalization_propagates_cancellation() -> anyhow::Result<()> {
        let mapper = mapper();
        let mut builder = builder(&mapper, &["body"])?;
        let body = mapper.schema().get_field("body")?;
        builder.add_document(ProcessedDoc {
            doc: doc!(body => "hello"),
            timestamp_opt: None,
            partition: 0,
            num_bytes: 5,
        })?;
        let kill_switch = KillSwitch::default();
        builder
            .controlled_directory
            .set_io_controls(IoControls::default().set_kill_switch(kill_switch.clone()));
        kill_switch.kill();
        assert!(builder.finalize().is_err());
        Ok(())
    }

    #[test]
    fn test_clustering_json_paths_preserve_documents() -> anyhow::Result<()> {
        let mapper = mapper();
        let mut builder = builder(&mapper, &["_dynamic.service", "labels.status"])?;
        let documents = [
            serde_json::json!({"body": "first", "service": "a", "labels": {"status": "ok"}}),
            serde_json::json!({"body": "second", "service": "b", "labels": {"status": "ok"}}),
            serde_json::json!({"body": "third", "service": "a", "labels": {"status": "error"}}),
            serde_json::json!({"body": "fourth", "service": "a", "labels": {"status": "error"}}),
        ];
        for document in documents {
            let num_bytes = document.to_string().len();
            let (partition, doc) = mapper
                .doc_from_json_obj(document.as_object().unwrap().clone(), num_bytes as u64)?;
            builder.add_document(ProcessedDoc {
                doc,
                partition,
                num_bytes,
                timestamp_opt: None,
            })?;
        }
        assert_eq!(
            builder.index_writer.clustering_group_sizes(),
            Some(vec![2, 1, 1])
        );
        let split = builder.finalize()?;
        let reader = split.index.reader()?;
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 4);
        let schema = split.index.schema();
        let body = schema.get_field("body")?;
        let mut bodies = Vec::new();
        for (doc_id, (service, status)) in
            [("a", "error"), ("a", "error"), ("a", "ok"), ("b", "ok")]
                .into_iter()
                .enumerate()
        {
            let document: TantivyDocument = searcher.doc(DocAddress::new(0, doc_id as u32))?;
            bodies.push(
                document
                    .get_first(body)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
            let stored: serde_json::Value = serde_json::from_str(&document.to_json(&schema))?;
            assert_eq!(stored["_dynamic"][0]["service"], service);
            assert_eq!(stored["labels"][0]["status"], status);
        }
        assert_eq!(bodies, ["third", "fourth", "first", "second"]);
        Ok(())
    }
}
