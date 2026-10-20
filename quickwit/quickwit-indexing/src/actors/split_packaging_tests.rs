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

use std::ops::RangeInclusive;

use quickwit_actors::Universe;
use quickwit_common::io::IoControls;
use quickwit_proto::search::{ListFieldsEntry, ListFieldsMetadata};
use quickwit_proto::types::{DocMappingUid, IndexUid, NodeId};
use tantivy::directory::MmapDirectory;
use tantivy::schema::{FAST, NumericOptions, STRING, Schema, TEXT, Type};
use tantivy::{DateTime, IndexBuilder, IndexSettings, doc};

use super::*;
use crate::actors::MergeExecutor;
use crate::controlled_directory::ControlledDirectory;
use crate::models::SplitAttrs;

#[test]
fn serialize_field_metadata_test() {
    let fields_metadata = vec![
        FieldMetadata {
            field_name: "test".to_string(),
            typ: Type::Str,
            stored: true,
            fast_size: Some(10u64.into()),
            term_dictionary_size: Some(10u64.into()),
            postings_size: Some(10u64.into()),
            positions_size: Some(10u64.into()),
        },
        FieldMetadata {
            field_name: "test2".to_string(),
            typ: Type::Str,
            stored: false,
            fast_size: None,
            term_dictionary_size: Some(10u64.into()),
            postings_size: Some(10u64.into()),
            positions_size: Some(10u64.into()),
        },
        FieldMetadata {
            field_name: "test3".to_string(),
            typ: Type::U64,
            stored: false,
            fast_size: Some(10u64.into()),
            term_dictionary_size: Some(10u64.into()),
            postings_size: Some(10u64.into()),
            positions_size: Some(10u64.into()),
        },
    ];

    let out = serialize_fields_metadata(&fields_metadata);

    let deserialized: Vec<ListFieldsEntry> =
        ListFieldsMetadata::deserialize(&out[..]).unwrap().entries;

    assert_eq!(fields_metadata.len(), deserialized.len());
    assert_eq!(deserialized[0].field_name, "test");
    assert_eq!(deserialized[0].field_type, ListFieldsType::Str as i32);
    assert!(deserialized[0].searchable);
    assert!(deserialized[0].aggregatable);

    assert_eq!(deserialized[1].field_name, "test2");
    assert_eq!(deserialized[1].field_type, ListFieldsType::Str as i32);
    assert!(deserialized[1].searchable);
    assert!(!deserialized[1].aggregatable);

    assert_eq!(deserialized[2].field_name, "test3");
    assert_eq!(deserialized[2].field_type, ListFieldsType::U64 as i32);
    assert!(deserialized[2].searchable);
    assert!(deserialized[2].aggregatable);
}

fn make_indexed_split_for_test(segment_timestamps: &[DateTime]) -> anyhow::Result<IndexedSplit> {
    let split_scratch_directory = TempDirectory::for_test();
    let mut schema_builder = Schema::builder();
    let text_field = schema_builder.add_text_field("text", TEXT);
    let timestamp_field = schema_builder.add_u64_field("timestamp", FAST);
    let tag_str = schema_builder.add_text_field("tag_str", STRING);
    let tag_many = schema_builder.add_text_field("tag_many", STRING);
    let tag_u64 = schema_builder.add_u64_field("tag_u64", NumericOptions::default().set_indexed());
    let tag_i64 = schema_builder.add_i64_field("tag_i64", NumericOptions::default().set_indexed());
    let tag_f64 = schema_builder.add_f64_field("tag_f64", NumericOptions::default().set_indexed());
    let tag_bool =
        schema_builder.add_bool_field("tag_bool", NumericOptions::default().set_indexed());
    let schema = schema_builder.build();
    let index_builder = IndexBuilder::new()
        .settings(IndexSettings::default())
        .schema(schema)
        .tokenizers(
            quickwit_query::create_default_quickwit_tokenizer_manager()
                .tantivy_manager()
                .clone(),
        )
        .fast_field_tokenizers(
            quickwit_query::get_quickwit_fastfield_normalizer_manager()
                .tantivy_manager()
                .clone(),
        );
    let index_directory = MmapDirectory::open(split_scratch_directory.path())?;
    let controlled_directory =
        ControlledDirectory::new(Box::new(index_directory), IoControls::default());
    let mut index_writer =
        index_builder.single_segment_index_writer(controlled_directory.clone(), 100_000_000)?;
    let mut timerange_opt: Option<RangeInclusive<DateTime>> = None;
    let mut num_docs = 0;
    for &timestamp in segment_timestamps {
        for num in 1..10 {
            let doc = doc!(
                text_field => format!("timestamp is {timestamp:?}"),
                timestamp_field => timestamp,
                tag_str => "value",
                tag_many => format!("many-{num}"),
                tag_u64 => 42u64,
                tag_i64 => -42i64,
                tag_f64 => -42.02f64,
                tag_bool => true,
            );
            index_writer.add_document(doc)?;
            num_docs += 1;
            timerange_opt = Some(
                timerange_opt
                    .map(|timestamp_range| {
                        let start = timestamp.min(*timestamp_range.start());
                        let end = timestamp.max(*timestamp_range.end());
                        RangeInclusive::new(start, end)
                    })
                    .unwrap_or_else(|| RangeInclusive::new(timestamp, timestamp)),
            )
        }
    }
    let index = index_writer.finalize()?;

    let node_id = NodeId::from_str("test-node");
    let index_uid = IndexUid::new_with_random_ulid("test-index");
    let source_id = "test-source".to_string();

    // TODO: In the future we would like that kind of segment flush to emit a new split,
    // but this will require work on tantivy.
    let indexed_split = IndexedSplit {
        split_attrs: SplitAttrs {
            node_id,
            index_uid,
            source_id,
            doc_mapping_uid: DocMappingUid::default(),
            split_id: "test-split".into(),
            partition_id: 17u64,
            num_docs,
            uncompressed_docs_size_in_bytes: num_docs * 15,
            time_range: timerange_opt,
            replaced_split_ids: Vec::new(),
            delete_opstamp: 0,
            num_merge_ops: 0,
        },
        index,
        split_scratch_directory,
        controlled_directory,
    };
    Ok(indexed_split)
}

fn get_tag_fields(schema: Schema, field_names: &[&str]) -> Vec<NamedField> {
    field_names
        .iter()
        .map(|field_name| {
            let field = schema.get_field(field_name).unwrap();
            let field_type = schema.get_field_entry(field).field_type().clone();
            NamedField {
                name: field_name.to_string(),
                field,
                field_type,
            }
        })
        .collect()
}

#[tokio::test]
async fn test_package_split() -> anyhow::Result<()> {
    quickwit_common::setup_logging_for_tests();
    let universe = Universe::with_accelerated_time();
    let (mailbox, _inbox) = universe.create_test_mailbox::<MergeExecutor>();
    let (observable_state_tx, _observable_state_rx) = tokio::sync::watch::channel(());
    let ctx = ActorContext::for_test(&universe, mailbox, observable_state_tx);
    let indexed_split = make_indexed_split_for_test(&[
        DateTime::from_timestamp_secs(1628203589),
        DateTime::from_timestamp_secs(1628203640),
    ])?;
    let tag_fields = get_tag_fields(
        indexed_split.index.schema(),
        &[
            "tag_str", "tag_many", "tag_u64", "tag_i64", "tag_f64", "tag_bool",
        ],
    );
    let split = package_split(indexed_split, &tag_fields, &ctx)?;
    assert_eq!(
        &split.tags.iter().map(|s| s.as_str()).collect::<Vec<&str>>(),
        &[
            "tag_bool!",
            "tag_bool:true",
            "tag_f64!",
            "tag_f64:-42.02",
            "tag_i64!",
            "tag_i64:-42",
            "tag_str!",
            "tag_str:value",
            "tag_u64!",
            "tag_u64:42"
        ]
    );
    assert_eq!(
        split.split_attrs.time_range,
        Some(DateTime::from_timestamp_secs(1628203589)..=DateTime::from_timestamp_secs(1628203640))
    );
    universe.assert_quit().await;
    Ok(())
}
