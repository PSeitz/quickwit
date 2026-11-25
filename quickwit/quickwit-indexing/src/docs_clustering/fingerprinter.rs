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

//! Fingerprint computation.
//!
//! A fingerprint contains one hash for each configured fingerprint policy, in configuration order.
//! Each policy can combine:
//! 1. structure fields, represented by sorted sets of leaf JSON paths;
//! 2. raw fields, represented by their exact JSON values;
//! 3. tokenized fields, represented by their token types.
//!
//! ```text
//! JSON document
//!     |
//!     +--> policy 0 fields --> hash 0
//!     +--> policy 1 fields --> hash 1
//!     +--> ...
//!     +--> policy N fields --> hash N
//!     |
//!     v
//! Fingerprint [hash 0, hash 1, ..., hash N]
//! ```
//!
//! Tokenized fields use the sequence of token types as a lightweight message template. This keeps
//! the stable structure of a value while ignoring volatile literals such as IDs, ports, UUIDs, or
//! IP addresses. By default, the first 50 tokens contribute to the fingerprint; `max_tokens` can
//! override this limit for each tokenized method.
//!
//! Examples:
//! ```text
//! "server started at 8080"
//!     -> Word Gap Word Gap Word Gap Number
//!
//! "server started at 9090"
//!     -> Word Gap Word Gap Word Gap Number
//! ```
//!
//! These two values produce the same tokenized signature and policy hash, so they can be ordered
//! together. A different shape produces a different signature:
//!
//! ```text
//! "connection from 1.2.3.4"
//!     -> Word Gap Word Gap IPv4
//! ```
//!
//! Raw fields preserve JSON types and nested structure, which is useful for dimensions such as
//! `service` or numeric status codes. Missing fields are encoded as absent so configured field
//! positions remain distinct within a policy hash.
use std::hash::Hasher;
use std::ops::Deref;
use std::sync::Arc;

use fnv::FnvHasher;
use itertools::Itertools;
use quickwit_config::{
    ClusteringMethod, ClusteringPolicy, DocsClusteringConfig, FingerprintPolicy, JsonPath,
};
use quickwit_doc_mapper::{
    ByteDictionary, QuickwitDoc, QuickwitLeafValue, QuickwitObject, QuickwitValue,
};
use smallvec::SmallVec;

use super::tokenize;

const PATH_COMPONENT_SEPARATOR: u8 = 0xF9;
const FIELD_ABSENT: u8 = 0xFA;
const FIELD_PRESENT: u8 = 0xFB;
const PATH_SEPARATOR: u8 = 0xFC;
const FIELD_BOUNDARY: u8 = 0xFD;
const TOKENIZED_TOKEN_SEPARATOR: u8 = 0xFE;
const DEFAULT_MAX_GROUPING_TOKENS: usize = 50;

// Inline 4 hashes to avoid heap allocations.
// This is usually enough for most use cases.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fingerprint(SmallVec<[u64; 4]>);

impl Deref for Fingerprint {
    type Target = [u64];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl Fingerprint {
    pub(crate) fn new<const N: usize>(hashes: [u64; N]) -> Self {
        let mut fingerprint = SmallVec::new();
        fingerprint.extend_from_slice(&hashes);
        Self(fingerprint)
    }
}

#[derive(Clone)]
pub struct Fingerprinter {
    config: Arc<DocsClusteringConfig>,
    policies: Arc<[FingerprintPolicy]>,
}

impl Fingerprinter {
    pub fn new(config: &DocsClusteringConfig) -> Self {
        let mut policies = Vec::new();
        for policy in &config.policies {
            match policy {
                ClusteringPolicy::Fingerprint { fingerprint } => {
                    policies.push(fingerprint.clone());
                }
            }
        }

        Self {
            config: Arc::new(config.clone()),
            policies: policies.into_boxed_slice().into(),
        }
    }

    pub fn config(&self) -> &DocsClusteringConfig {
        &self.config
    }

    /// Hashes original input fields. Mapping stores converted values in separate bindings.
    pub fn fingerprint(&self, doc: &QuickwitDoc) -> Fingerprint {
        let mut fingerprint = SmallVec::new();
        for policy in self.policies.iter() {
            let mut hasher = FnvHasher::default();

            for method in policy.fingerprint.iter() {
                match method {
                    ClusteringMethod::Structure { exclude } => {
                        hash_structure(
                            doc.as_object(),
                            doc.dictionary(),
                            exclude,
                            &mut Vec::new(),
                            &mut hasher,
                        );
                    }
                    ClusteringMethod::Raw { path } => {
                        hash_raw_value(doc, path, &mut hasher);
                    }
                    ClusteringMethod::Tokenized { path, max_tokens } => {
                        hash_string_tokenized(doc, path, *max_tokens, &mut hasher);
                    }
                }
            }

            fingerprint.push(hasher.finish());
        }

        Fingerprint(fingerprint)
    }
}

fn hash_structure<'a>(
    object: QuickwitObject<'a>,
    dictionary: &'a ByteDictionary,
    exclude: &[JsonPath],
    current: &mut Vec<&'a str>,
    hasher: &mut FnvHasher,
) {
    // Sorting siblings yields complete leaf paths in lexicographic order. Arrays count as leaves.
    for (key, value) in object
        .iter(dictionary)
        .sorted_unstable_by_key(|(key, _)| *key)
    {
        current.push(key);
        if exclude.iter().any(|path| path[..] == current[..]) {
            current.pop();
            continue;
        }
        if let QuickwitValue::Object(child) = value {
            hash_structure(child, dictionary, exclude, current, hasher);
        } else {
            for component in current.iter() {
                hasher.write(component.as_bytes());
                hasher.write_u8(PATH_COMPONENT_SEPARATOR);
            }
            hasher.write_u8(PATH_SEPARATOR);
        }
        current.pop();
    }
}

fn hash_raw_value(doc: &QuickwitDoc, path: &JsonPath, hasher: &mut FnvHasher) {
    if let Some(value) = doc.get_path(path) {
        hasher.write_u8(FIELD_PRESENT);
        value.hash_json(
            doc.dictionary(),
            hasher,
            Hasher::write_usize,
            |hasher, num| hasher.write(num.to_string().as_bytes()),
        );
    } else {
        hasher.write_u8(FIELD_ABSENT);
    }
    hasher.write_u8(FIELD_BOUNDARY);
}

fn hash_string_tokenized(
    doc: &QuickwitDoc,
    path: &JsonPath,
    max_tokens: Option<usize>,
    hasher: &mut FnvHasher,
) {
    if let Some(QuickwitValue::Leaf(QuickwitLeafValue::String(id))) = doc.get_path(path) {
        hasher.write_u8(FIELD_PRESENT);
        let max_tokens = max_tokens.unwrap_or(DEFAULT_MAX_GROUPING_TOKENS);
        for span in tokenize(doc.dictionary().get_str(id)).take(max_tokens) {
            hasher.write_u8(span.token_type as u8);
            hasher.write_u8(TOKENIZED_TOKEN_SEPARATOR);
        }
    } else {
        hasher.write_u8(FIELD_ABSENT);
    }
    hasher.write_u8(FIELD_BOUNDARY);
}

#[cfg(test)]
mod tests {
    use quickwit_config::DocsClusteringConfig;
    use quickwit_doc_mapper::{DocMapper, QuickwitDoc, QuickwitDocBatch};
    use serde_json::Value as JsonValue;

    use super::Fingerprinter;

    fn parse(s: &str) -> QuickwitDoc {
        let mapper: DocMapper = serde_json::from_str("{}").unwrap();
        let mut batch = QuickwitDocBatch::default();
        batch.parse(s.as_bytes()).unwrap();
        batch.map(&mapper, |_| ()).next().unwrap().1.unwrap().2
    }

    fn test_docs_clustering_config() -> DocsClusteringConfig {
        docs_clustering_config(serde_json::json!([
            {
                "fingerprint": [{
                    "kind": "structure",
                    "exclude": ["tag", "custom"]
                }]
            },
            {
                "fingerprint": [{
                    "path": "message",
                    "kind": "tokenized"
                },
                {
                    "path": "service",
                    "kind": "raw"
                }]
            }
        ]))
    }

    fn test_fingerprinter() -> Fingerprinter {
        let docs_clustering_config = test_docs_clustering_config();
        Fingerprinter::new(&docs_clustering_config)
    }

    fn docs_clustering_config(json_value: JsonValue) -> DocsClusteringConfig {
        serde_json::from_value(json_value).unwrap()
    }

    #[test]
    fn fingerprint_is_independent_of_batch_ids() {
        let source = r#"{"service":"auth","message":"münchen 42","payload":{"z":"münchen 42","a":["另一"]}}"#;
        let fingerprinter = test_fingerprinter();
        let expected = fingerprinter.fingerprint(&parse(source));
        let mapper: DocMapper = serde_json::from_str("{}").unwrap();
        let mut batch = QuickwitDocBatch::default();
        batch.parse(br#"{"unused":"prefix"}"#).unwrap();
        batch.parse(source.as_bytes()).unwrap();
        let (fp, _, _) = batch
            .map(&mapper, |doc| fingerprinter.fingerprint(doc))
            .last()
            .unwrap()
            .1
            .unwrap();
        assert_eq!(fp, expected);
    }

    #[test]
    fn configured_fingerprinter_returns_config() {
        let docs_clustering_config = test_docs_clustering_config();
        let fingerprinter = test_fingerprinter();
        assert_eq!(fingerprinter.config(), &docs_clustering_config);
    }

    #[test]
    fn identical_logs_have_equal_fingerprint() {
        let fingerprinter = test_fingerprinter();
        let doc = parse(r#"{"message":"server started at 8080","service":"api"}"#);
        assert_eq!(
            fingerprinter.fingerprint(&doc),
            fingerprinter.fingerprint(&doc)
        );
    }

    #[test]
    fn dotted_key_and_nested_path_have_different_schema_fingerprints() {
        let fingerprinter = test_fingerprinter();
        let dotted_key_doc = parse(r#"{"a.b":1}"#);
        let nested_path_doc = parse(r#"{"a":{"b":1}}"#);
        let dotted_key_fingerprint = fingerprinter.fingerprint(&dotted_key_doc);
        let nested_path_fingerprint = fingerprinter.fingerprint(&nested_path_doc);

        assert_ne!(dotted_key_fingerprint[0], nested_path_fingerprint[0]);
        assert_eq!(dotted_key_fingerprint[1], nested_path_fingerprint[1]);
    }

    #[test]
    fn same_message_template_has_equal_fingerprint() {
        let fingerprinter = test_fingerprinter();
        let doc1 = parse(r#"{"message":"server started at 8080","service":"api"}"#);
        let doc2 = parse(r#"{"message":"server started at 9090","service":"api"}"#);
        assert_eq!(
            fingerprinter.fingerprint(&doc1),
            fingerprinter.fingerprint(&doc2)
        );
    }

    #[test]
    fn different_message_template_changes_grouping_fingerprint_only() {
        let fingerprinter = test_fingerprinter();
        let doc1 = parse(r#"{"message":"server started at 8080","service":"api"}"#);
        let doc2 = parse(r#"{"message":"connection from 1.2.3.4","service":"api"}"#);
        let doc1_fingerprint = fingerprinter.fingerprint(&doc1);
        let doc2_fingerprint = fingerprinter.fingerprint(&doc2);
        assert_eq!(doc1_fingerprint[0], doc2_fingerprint[0]);
        assert_ne!(doc1_fingerprint[1], doc2_fingerprint[1]);
    }

    #[test]
    fn tokenized_field_respects_hardcoded_grouping_token_limit() {
        let docs_clustering_config = docs_clustering_config(serde_json::json!([
            {
                "fingerprint": [{
                    "kind": "structure"
                }]
            },
            {
                "fingerprint": [{
                    "path": "message",
                    "kind": "tokenized"
                }]
            }
        ]));
        let fingerprinter = Fingerprinter::new(&docs_clustering_config);
        let prefix = "alpha ".repeat(25);
        let doc1 = parse(&format!(r#"{{"message":"{prefix}123"}}"#));
        let doc2 = parse(&format!(r#"{{"message":"{prefix}beta"}}"#));
        assert_eq!(
            fingerprinter.fingerprint(&doc1)[1],
            fingerprinter.fingerprint(&doc2)[1]
        );
    }

    #[test]
    fn different_service_changes_grouping_fingerprint_only() {
        let fingerprinter = test_fingerprinter();
        let doc1 = parse(r#"{"message":"server started at 8080","service":"api"}"#);
        let doc2 = parse(r#"{"message":"server started at 8080","service":"worker"}"#);
        let doc1_fingerprint = fingerprinter.fingerprint(&doc1);
        let doc2_fingerprint = fingerprinter.fingerprint(&doc2);
        assert_eq!(doc1_fingerprint[0], doc2_fingerprint[0]);
        assert_ne!(doc1_fingerprint[1], doc2_fingerprint[1]);
    }

    #[test]
    fn ignored_custom_shape_does_not_change_fingerprint() {
        let fingerprinter = test_fingerprinter();
        let doc1 =
            parse(r#"{"message":"server started at 8080","service":"api","custom":{"a":1}}"#);
        let doc2 =
            parse(r#"{"message":"server started at 8080","service":"api","custom":{"b":2}}"#);
        assert_eq!(
            fingerprinter.fingerprint(&doc1),
            fingerprinter.fingerprint(&doc2)
        );
    }

    #[test]
    fn extra_non_ignored_shape_changes_schema_fingerprint_only() {
        let fingerprinter = test_fingerprinter();
        let doc1 = parse(r#"{"message":"server started at 8080","service":"api"}"#);
        let doc2 = parse(r#"{"message":"server started at 8080","service":"api","host":"web-1"}"#);
        let doc1_fingerprint = fingerprinter.fingerprint(&doc1);
        let doc2_fingerprint = fingerprinter.fingerprint(&doc2);
        assert_ne!(doc1_fingerprint[0], doc2_fingerprint[0]);
        assert_eq!(doc1_fingerprint[1], doc2_fingerprint[1]);
    }

    #[test]
    fn configured_raw_field_changes_grouping_fingerprint_only() {
        let docs_clustering_config = docs_clustering_config(serde_json::json!([
            {
                "fingerprint": [{
                    "kind": "structure"
                }]
            },
            {
                "fingerprint": [{
                    "path": "host",
                    "kind": "raw"
                }]
            }
        ]));
        let fingerprinter = Fingerprinter::new(&docs_clustering_config);
        let doc1 = parse(r#"{"message":"same","host":"web-1"}"#);
        let doc2 = parse(r#"{"message":"same","host":"web-2"}"#);
        let doc1_fingerprint = fingerprinter.fingerprint(&doc1);
        let doc2_fingerprint = fingerprinter.fingerprint(&doc2);
        assert_eq!(doc1_fingerprint[0], doc2_fingerprint[0]);
        assert_ne!(doc1_fingerprint[1], doc2_fingerprint[1]);
    }

    #[test]
    fn raw_field_hashes_non_string_values() {
        let docs_clustering_config = docs_clustering_config(serde_json::json!([
            {
                "fingerprint": [{
                    "kind": "structure"
                }]
            },
            {
                "fingerprint": [{
                    "path": "status",
                    "kind": "raw"
                }]
            }
        ]));
        let fingerprinter = Fingerprinter::new(&docs_clustering_config);
        let doc1 = parse(r#"{"status":200}"#);
        let doc2 = parse(r#"{"status":500}"#);
        let doc1_fingerprint = fingerprinter.fingerprint(&doc1);
        let doc2_fingerprint = fingerprinter.fingerprint(&doc2);
        assert_eq!(doc1_fingerprint[0], doc2_fingerprint[0]);
        assert_ne!(doc1_fingerprint[1], doc2_fingerprint[1]);
    }

    #[test]
    fn raw_field_preserves_json_type_and_structure_boundaries() {
        let docs_clustering_config = docs_clustering_config(serde_json::json!([
            {
                "fingerprint": [{
                    "path": "value",
                    "kind": "raw"
                }]
            }
        ]));
        let fingerprinter = Fingerprinter::new(&docs_clustering_config);
        let docs = [
            parse(r#"{"value":"null"}"#),
            parse(r#"{"value":null}"#),
            parse(r#"{"value":""}"#),
            parse(r#"{"value":[]}"#),
            parse(r#"{"value":{}}"#),
            parse(r#"{"value":false}"#),
            parse(r#"{"value":-1}"#),
            parse(r#"{"value":18446744073709551615}"#),
        ];
        let fingerprints: Vec<_> = docs
            .iter()
            .map(|doc| fingerprinter.fingerprint(doc))
            .collect();

        for (left_idx, left_fingerprint) in fingerprints.iter().enumerate() {
            for right_fingerprint in &fingerprints[left_idx + 1..] {
                assert_ne!(left_fingerprint, right_fingerprint);
            }
        }
    }

    #[test]
    fn absent_grouping_values_preserve_field_position() {
        let docs_clustering_config = docs_clustering_config(serde_json::json!([
            {
                "fingerprint": [{
                    "kind": "structure"
                }]
            },
            {
                "fingerprint": [{
                    "path": "a",
                    "kind": "raw"
                },
                {
                    "path": "b",
                    "kind": "raw"
                }]
            }
        ]));
        let fingerprinter = Fingerprinter::new(&docs_clustering_config);
        let doc1 = parse(r#"{"a":"x","b":null}"#);
        let doc2 = parse(r#"{"a":null,"b":"x"}"#);
        let doc1_fingerprint = fingerprinter.fingerprint(&doc1);
        let doc2_fingerprint = fingerprinter.fingerprint(&doc2);

        assert_eq!(doc1_fingerprint[0], doc2_fingerprint[0]);
        assert_ne!(doc1_fingerprint[1], doc2_fingerprint[1]);
    }
}
