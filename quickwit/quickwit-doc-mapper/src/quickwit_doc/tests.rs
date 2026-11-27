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

use std::collections::BTreeMap;

use itertools::Itertools;
use quickwit_common::shared_consts::FIELD_PRESENCE_FIELD_NAME;
use serde_json::json;
use tantivy::schema::{Document, OwnedValue, Schema, Value};

use super::QuickwitValue::Leaf;
use super::*;
use crate::{
    DOCUMENT_SIZE_FIELD_NAME, DYNAMIC_FIELD_NAME, DocMapper, JsonObject, RoutingExpr,
    SOURCE_FIELD_NAME,
};

fn sort_object_keys(value: &mut OwnedValue) {
    match value {
        OwnedValue::Object(entries) => {
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            entries
                .iter_mut()
                .for_each(|(_, child)| sort_object_keys(child));
        }
        OwnedValue::Array(items) => items.iter_mut().for_each(sort_object_keys),
        _ => (),
    }
}

fn named_fields(doc: &impl Document, schema: &Schema) -> BTreeMap<String, Vec<OwnedValue>> {
    let mut fields = doc.to_named_doc(schema).0;
    for values in fields.values_mut() {
        values.iter_mut().for_each(sort_object_keys);
    }
    if let Some(hashes) = fields.get_mut(FIELD_PRESENCE_FIELD_NAME) {
        hashes.sort_unstable_by_key(|value| match value {
            OwnedValue::U64(hash) => *hash,
            _ => panic!("invalid field presence hash: {value:?}"),
        });
    }
    fields
}

fn map_one(mapper: &DocMapper, input: &str) -> Result<(u64, QuickwitDoc), crate::DocParsingError> {
    let mut batch = QuickwitDocBatch::default();
    batch.parse(input.as_bytes()).unwrap();
    batch.map(mapper).next().unwrap().1
}

fn assert_parity(mapper: &DocMapper, input: &str) -> BTreeMap<String, Vec<OwnedValue>> {
    let (expected_partition, expected_doc) = mapper.doc_from_json_str(input).unwrap();
    let (partition, mapped_doc) = map_one(mapper, input).unwrap();
    assert_eq!(partition, expected_partition, "{input}");
    let schema = mapper.schema();
    let mapped_fields = named_fields(&mapped_doc, &schema);
    assert_eq!(
        mapped_fields,
        named_fields(&expected_doc, &schema),
        "{input}"
    );
    mapped_fields
}

fn assert_error_parity(mapper: &DocMapper, input: &str) {
    let expected = mapper.doc_from_json_str(input).unwrap_err();
    let actual = map_one(mapper, input).unwrap_err();
    assert_eq!(actual.to_string(), expected.to_string(), "{input}");
}

#[test]
fn batch_shares_entries_and_rolls_back_errors() -> anyhow::Result<()> {
    let (first, second) = (
        br#"{"tag":["tag",{"tag":"\u0074ag"}]}"#,
        br#"{"value":"tag"}"#,
    );
    let mut batch = QuickwitDocBatch::default();
    batch.parse(first)?;
    for bad in [
        "",
        "{",
        r#"{"secret":["rejected",]}"#,
        r#"{"a":0} false"#,
        "null",
        "[]",
    ] {
        assert!(batch.parse(bad.as_bytes()).is_err(), "{bad}");
    }
    assert!(batch.push_serialize(&json!(["not an object"]), 10).is_err());
    batch.parse(second)?;
    batch.push_serialize(&json!({"value": "tag"}), 300)?;
    batch.push_plain_text("tag")?;

    let lens: Vec<usize> = batch.docs.iter().map(|(len, _)| *len).collect();
    assert_eq!(lens, [first.len(), second.len(), 300, 3]);
    let doc_obj = |idx: usize| decode_root_object(&batch.data.nodes, batch.docs[idx].1);
    let tag_id = doc_obj(0).entries().next().unwrap().0;
    let tag = QuickwitLeafValue::String(tag_id);
    assert!(matches!(doc_obj(1).get("value", &batch.data.dictionary), Some(Leaf(v)) if v == tag));
    assert!(matches!(doc_obj(2).get("value", &batch.data.dictionary), Some(Leaf(v)) if v == tag));
    assert!(matches!(
        doc_obj(3).get("plain_text", &batch.data.dictionary),
        Some(Leaf(v)) if v == tag
    ));
    assert_eq!(doc_obj(1).entries().next().unwrap().0, 1);

    // Rejected mapping rolls back only newly interned binary entries.
    let mapper: DocMapper = serde_json::from_value(json!({"field_mappings":[
        {"name":"data","type":"array<bytes>","input_format":"hex"}
    ]}))?;
    let mut batch = QuickwitDocBatch::default();
    let invalid = br#"{"data":["ff","aa","invalid"]}"#;
    batch.parse(br#"{"data":"ff"}"#)?;
    batch.parse(invalid)?;
    batch.parse(br#"{"data":"cc"}"#)?;
    let mut mapped = batch.map(&mapper);
    let (_, first) = mapped.next().unwrap().1?;
    let (len, error) = mapped.next().unwrap();
    assert_eq!(len, invalid.len());
    assert!(matches!(error, Err(crate::DocParsingError::ValueError(path, _)) if path == "data"));
    let (_, last) = mapped.next().unwrap().1?;
    assert!(std::ptr::eq(first.dictionary(), last.dictionary()));
    let Some(Leaf(QuickwitLeafValue::String(text_id))) =
        last.as_object().get("data", last.dictionary())
    else {
        panic!("expected original hex text");
    };
    assert_eq!(last.dictionary().get_bytes(text_id + 1), &[0xff]);
    assert_eq!(last.dictionary().get_bytes(text_id + 2), &[0xcc]);

    // Reusing the batch starts fresh storage; long values bypass dedup while long keys deduplicate.
    let long_key = "k".repeat(300);
    let long_val = "v".repeat(300);
    let bad_long = format!(r#"{{"bad":"{long_val}",}}"#);
    assert!(batch.parse(bad_long.as_bytes()).is_err());
    let dup_long_key = format!(r#"{{"{long_key}":"first","{long_key}":"{long_val}"}}"#);
    batch.parse(dup_long_key.as_bytes())?;
    let second_doc = batch.map(&mapper).next().unwrap().1?.1;
    assert!(!std::ptr::eq(first.dictionary(), second_doc.dictionary()));
    let entries: Vec<_> = second_doc
        .as_object()
        .iter(second_doc.dictionary())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, long_key);
    assert!(matches!(
        entries[0].1,
        Leaf(QuickwitLeafValue::String(id)) if second_doc.dictionary().get_str(id) == long_val
    ));
    Ok(())
}

#[test]
fn dictionary_truncate_freeze_and_deserialization() {
    let mut dict = ByteDictionary::default();
    let first = dict.intern_str("first").unwrap();
    let checkpoint = dict.len();
    let removed = dict.intern_str("removed").unwrap();
    dict.truncate(checkpoint);
    let binary = dict.intern(&[0xff]).unwrap();
    assert_eq!(binary, removed);
    assert_eq!(dict.get_str(first), "first");
    let text = dict.intern_str("☃").unwrap();
    assert_eq!(dict.get_str(text), "☃");
    dict.freeze();
    assert_eq!(dict.get_bytes(binary), &[0xff]);

    let empty_mapper: DocMapper = serde_json::from_str("{}").unwrap();
    let (_, doc) = map_one(
        &empty_mapper,
        r#"{"first":1,"custom":{"k":false,"k":true},"values":[-1,2,1.5,-0.0,null],"custom.k":false,"first":null}"#,
    )
    .unwrap();
    let keys: Vec<_> = doc
        .as_object()
        .iter(doc.dictionary())
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys, ["first", "custom", "values", "custom.k"]);
    assert!(matches!(
        doc.get_path(&["custom".into(), "k".into()]),
        Some(Leaf(QuickwitLeafValue::Bool(true)))
    ));
    assert!(matches!(
        doc.get_path(&["custom.k".into()]),
        Some(Leaf(QuickwitLeafValue::Bool(false)))
    ));
    let Some(QuickwitValue::Array(items)) = doc.get_path(&["values".into()]) else {
        panic!()
    };
    let leaves: Vec<_> = items
        .iter()
        .map(|item| match item {
            Leaf(leaf) => leaf,
            _ => panic!(),
        })
        .collect();
    assert_eq!(
        leaves[..3],
        [
            QuickwitLeafValue::I64(-1),
            QuickwitLeafValue::U64(2),
            QuickwitLeafValue::F64(1.5),
        ]
    );
    assert!(matches!(leaves[3], QuickwitLeafValue::F64(n) if n.is_sign_negative() && n == 0.0));

    // Wide objects (>= 256 keys) keep the last value for duplicate keys in sorted order.
    let fields = (0..512).map(|i| format!(r#""k{i}":{i}"#)).join(",");
    let (_, wide) = map_one(
        &empty_mapper,
        &format!(r#"{{{fields},"k0":true,"k511":true}}"#),
    )
    .unwrap();
    assert_eq!(wide.as_object().len(), 512);
    for key in ["k0", "k511"] {
        assert!(matches!(
            wide.get_path(&[key.into()]),
            Some(Leaf(QuickwitLeafValue::Bool(true)))
        ));
    }
}

#[test]
fn routing_matches_json() {
    let source = r#"{
        "key.with.dot":{"a":[{"a":true,"b":1.0},[],{}],"b":27,"z":null,"b":-1},
        "nil":null,
        "nums":[0,-0,1,-1,-9223372036854775808,18446744073709551615,18446744073709551616,-0.0,5e-10],
        "tenant":{"name":"münchen","nested":["literal",false]}
    }"#;
    let json: JsonObject = serde_json::from_str(source).unwrap();
    let mapper: DocMapper = serde_json::from_str("{}").unwrap();
    let mut batch = QuickwitDocBatch::default();
    batch.parse(br#"{"prefix":"offset_ids"}"#).unwrap();
    batch.parse(source.as_bytes()).unwrap();
    let (_, doc) = batch.map(&mapper).last().unwrap().1.unwrap();
    for expr in [
        "",
        r"key\.with\.dot",
        "tenant.name",
        "tenant.nested.name",
        "absent",
        "nil",
        "nums",
        r"hash_mod((key\.with\.dot,tenant), 5),tenant.nested",
    ] {
        let routing = RoutingExpr::new(expr).unwrap();
        assert_eq!(routing.eval_hash(&doc), routing.eval_hash(&json), "{expr}");
    }
}

#[test]
fn scalar_types_coercion_and_errors_match_json_mapper() {
    let mapper: DocMapper = serde_json::from_value(json!({
        "mode": "strict",
        "field_mappings": [
            {"name": "i64", "type": "i64"},
            {"name": "i64_strict", "type": "i64", "coerce": false},
            {"name": "u64", "type": "u64"},
            {"name": "f64", "type": "f64"},
            {"name": "bool", "type": "bool"},
            {"name": "text", "type": "text"},
            {"name": "ip", "type": "ip"},
            {"name": "b64", "type": "bytes"},
            {"name": "hex", "type": "bytes", "input_format": "hex"},
            {"name": "ts", "type": "datetime"},
            {"name": "ts_fmt", "type": "datetime", "input_formats": ["%Y/%m/%d"]},
            {"name": "json", "type": "json"},
            {"name": "nested", "type": "object", "field_mappings": [{"name": "ips", "type": "array<ip>"}]}
        ]
    }))
    .unwrap();
    for input in [
        r#"{"i64":-1,"u64":"18446744073709551615","f64":"-1.25e2","bool":true,"text":"hi"}"#,
        r#"{"i64":"42","i64_strict":1,"u64":0,"f64":-0.0,"ip":"127.0.0.1","b64":"aGVsbG8="}"#,
        r#"{"hex":"00fF","ts":"2012-05-21T12:09:14.123456789Z","ts_fmt":"2025/01/30"}"#,
        r#"{"ts":1700000000123456789,"json":{"a":[null,-1,0,18446744073709551615,"2020-08-01T06:30:10.123456789+04:30","2262-04-11T23:47:16.854775808Z"]}}"#,
        r#"{"i64":null,"nested":null}"#,
        r#"{"nested":[null]}"#,
        r#"{"nested":{"ips":["2001:db8::1",null,"127.0.0.1"]}}"#,
    ] {
        assert_parity(&mapper, input);
    }
    for invalid in [
        r#"{"i64_strict":"42"}"#,
        r#"{"i64_strict":1.0}"#,
        r#"{"i64":18446744073709551615}"#,
        r#"{"i64":[1]}"#,
        r#"{"u64":-1}"#,
        r#"{"bool":"true"}"#,
        r#"{"ip":"bad"}"#,
        r#"{"b64":"*"}"#,
        r#"{"hex":"xyz"}"#,
        r#"{"ts":true}"#,
        r#"{"json":"not-an-object"}"#,
        r#"{"nested":false}"#,
        r#"{"nested":{"ips":["::1","bad"]}}"#,
        r#"{"nested":{"unknown":1}}"#,
        r#"{"unknown":1}"#,
    ] {
        assert_error_parity(&mapper, invalid);
    }
}

#[test]
fn stored_source_dynamic_concatenate_and_batch_dedup() -> anyhow::Result<()> {
    fn decoded(doc: &QuickwitDoc) -> Vec<&[u8]> {
        doc.iter_fields_and_values()
            .filter_map(|(_, value)| value.as_bytes())
            .collect()
    }
    let mapper: DocMapper = serde_json::from_value(json!({
        "mode":"dynamic", "store_source":true, "index_field_presence":true,
        "store_document_size":true, "partition_key":"user.id",
        "field_mappings":[
            {"name":"user", "type":"object", "field_mappings":[{"name":"id", "type":"u64"}]},
            {"name":"user.id", "type":"text"},
            {"name":"base64", "type":"array<bytes>", "input_format":"base64"},
            {"name":"hex", "type":"array<bytes>", "input_format":"hex"},
            {"name":"address", "type":"ip"},
            {"name":"entries", "type":"array<json>"},
            {"name":"sequence", "type":"tie_breaker"},
            {"name":"mapped", "type":"concatenate", "concatenate_fields":["entries","user.id"]},
            {"name":"all", "type":"concatenate", "concatenate_fields":["entries","user.id"], "include_dynamic_fields":true}
        ]
    }))?;
    let ts = "2024-03-21T15:04:05.120+02:00";
    let inputs = [
        json!({
            "user.id": "café",
            "user": {"id": "007", "z": null, "x.y": [{"a": 2, "b": 1}, null]},
            "base64": ["AP+A", "Y2Fmw6k=", ""],
            "address": "192.0.2.10",
            "entries": [{"data": [-3, 7, null, [u64::MAX, ts], {"flag": false}]}, null, {}],
            "sequence": 42
        })
        .to_string(),
        r#"{"hex":["00ff80","636166C3A9",""],"address":"::ffff:c000:020a","café":true,"user":{"id":9}}"#
            .to_string(),
    ];
    let mut batch = QuickwitDocBatch::default();
    for input in &inputs {
        batch.parse(input.as_bytes())?;
    }
    let mut mapped = batch.map(&mapper);
    let (p1, first) = mapped.next().unwrap().1?;
    let (p2, second) = mapped.next().unwrap().1?;
    let (base64, hex) = (decoded(&first), decoded(&second));
    assert_eq!(base64, [b"\x00\xff\x80".as_slice(), "café".as_bytes(), b""]);
    for (&left, right) in base64.iter().zip(hex) {
        assert!(std::ptr::eq(left, right));
    }
    let (key, _) = second
        .as_object()
        .iter(second.dictionary())
        .find(|(key, _)| *key == "café")
        .unwrap();
    assert!(std::ptr::eq(key.as_bytes(), base64[1]));

    for ((partition, doc), input) in [(p1, first), (p2, second)].iter().zip(&inputs) {
        let (expected_partition, expected_doc) = mapper.doc_from_json_str(input)?;
        assert_eq!(*partition, expected_partition);
        let mapped_fields = named_fields(doc, &mapper.schema());
        assert_eq!(mapped_fields, named_fields(&expected_doc, &mapper.schema()));
        assert!(mapped_fields.contains_key(SOURCE_FIELD_NAME));
        assert!(mapped_fields.contains_key(DYNAMIC_FIELD_NAME));
        assert!(mapped_fields.contains_key(FIELD_PRESENCE_FIELD_NAME));
        assert_eq!(
            mapped_fields[DOCUMENT_SIZE_FIELD_NAME],
            [OwnedValue::U64(input.len() as u64)]
        );
    }
    assert!(!assert_parity(&mapper, r#"{"user":{"id":9}}"#).contains_key(DYNAMIC_FIELD_NAME));

    let strict_tb: DocMapper = serde_json::from_value(json!({
        "mode":"strict", "field_mappings":[{"name":"sequence", "type":"tie_breaker"}]
    }))?;
    assert!(assert_parity(&strict_tb, r#"{"sequence":null}"#).is_empty());
    assert_error_parity(&strict_tb, r#"{"sequence":42}"#);
    assert_error_parity(&strict_tb, r#"{"sequence":[null]}"#);
    Ok(())
}
