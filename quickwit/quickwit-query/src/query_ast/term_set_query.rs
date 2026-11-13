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

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use tantivy::Term;
use tantivy::query::{BooleanQuery, ConstScoreQuery};
use tantivy::schema::{IndexRecordOption, Type};

use super::regex_query::ResolvedRegex;
use crate::InvalidQuery;
use crate::query_ast::{
    BoolQuery, BuildTantivyAst, BuildTantivyAstContext, QueryAst, TantivyQueryAst, TermQuery,
};

/// TermSetQuery matches the same document set as if it was a union of
/// the equivalent set of TermQueries.
///
/// The text will be used as is, untokenized.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TermSetQuery {
    pub terms_per_field: BTreeMap<String, BTreeSet<String>>,
}

impl TermSetQuery {
    fn has_fast_only_field(&self, context: &BuildTantivyAstContext) -> bool {
        for full_path in self.terms_per_field.keys() {
            if let Some((_, field_entry, _)) =
                super::utils::find_field_or_hit_dynamic(full_path, context.schema)
                && field_entry.is_fast()
                && !field_entry.is_indexed()
            {
                return true;
            }
        }
        false
    }

    fn build_bool_query(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<TantivyQueryAst, InvalidQuery> {
        let should_clauses = self
            .terms_per_field
            .iter()
            .flat_map(|(full_path, values)| {
                values.iter().map(|value| {
                    QueryAst::Term(TermQuery {
                        field: full_path.to_string(),
                        value: value.to_string(),
                    })
                })
            })
            .collect();

        let bool_query = BoolQuery {
            should: should_clauses,
            ..Default::default()
        };

        bool_query.build_tantivy_ast_impl(context)
    }

    /// Returns the regexes used for indexed text terms, for dictionary and postings warmup.
    /// Fast-only fields use the boolean query path and need no regex warmup.
    pub fn to_regexes(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<Vec<ResolvedRegex>, InvalidQuery> {
        if self.has_fast_only_field(context) {
            return Ok(Vec::new());
        }
        let (regexes, _) = self.resolve_terms(context)?;
        Ok(regexes)
    }

    fn resolve_terms(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<(Vec<ResolvedRegex>, Vec<Term>), InvalidQuery> {
        let mut text_terms = BTreeMap::new();
        let mut other_terms = Vec::new();
        for term in self.make_term_iterator(context)? {
            let bytes = term.serialized_value_bytes();
            let (json_path, text_bytes) = if term.typ() == Type::Str {
                (None, bytes)
            } else if term.value().json_path_type() == Some(Type::Str) {
                // JSON dictionary keys contain the path, a NUL terminator, a type byte,
                // and the value. Keep the path and string type outside the regex.
                let prefix_len = bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .expect("JSON string terms have a path terminator")
                    + 2;
                (Some(bytes[..prefix_len].to_vec()), &bytes[prefix_len..])
            } else {
                other_terms.push(term);
                continue;
            };
            let text = std::str::from_utf8(text_bytes).expect("text terms contain valid UTF-8");
            text_terms
                .entry((term.field(), json_path))
                .or_insert_with(BTreeSet::new)
                .insert(regex::escape(text));
        }
        let regexes = text_terms
            .into_iter()
            .map(|((field, json_path), terms)| ResolvedRegex {
                field,
                json_path,
                // FST regexes match the entire term. Escaping preserves literal matching,
                // including empty strings. TermQuery normalization has already been applied,
                // so do not apply RegexQuery's automatic case-insensitive matching.
                regex: terms.into_iter().collect::<Vec<_>>().join("|"),
            })
            .collect();
        Ok((regexes, other_terms))
    }

    fn build_term_disjunction(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<TantivyQueryAst, InvalidQuery> {
        let (regexes, terms) = self.resolve_terms(context)?;
        let mut term_queries: Vec<Box<dyn crate::TantivyQuery>> = terms
            .into_iter()
            .map(|term| {
                Box::new(tantivy::query::TermQuery::new(
                    term,
                    IndexRecordOption::Basic,
                )) as Box<dyn crate::TantivyQuery>
            })
            .collect();
        for regex in regexes {
            term_queries.push(regex.build_tantivy_ast()?.into());
        }
        // Score the union, not each term: matching several terms still contributes only 1.0.
        // Both regexes and the remaining term queries use Basic postings.
        let disjunction = BooleanQuery::union(term_queries);
        Ok(ConstScoreQuery::new(Box::new(disjunction), 1.0).into())
    }

    fn make_term_iterator(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<HashSet<Term>, InvalidQuery> {
        let mut terms: HashSet<Term> = HashSet::default();

        for (full_path, values) in &self.terms_per_field {
            for value in values {
                // Mapping a text (field, value) is non-trivial:
                // It depends on the schema of course, and can actually result in a disjunction of
                // multiple terms if the query targets a dynamic field (due to the
                // different types).
                //
                // Here, we ensure the logic is the same as for a TermQuery, by creating the term
                // query and extracting the terms from the resulting `TermQuery`.
                let term_query = TermQuery {
                    field: full_path.to_string(),
                    value: value.to_string(),
                };
                let ast = term_query.build_tantivy_ast_call(context)?;
                let tantivy_query: Box<dyn crate::TantivyQuery> = ast.simplify().into();
                tantivy_query.query_terms(&mut |term, _| {
                    terms.insert(term.clone());
                });
            }
        }
        Ok(terms)
    }
}

impl BuildTantivyAst for TermSetQuery {
    fn build_tantivy_ast_impl(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<TantivyQueryAst, InvalidQuery> {
        if self.has_fast_only_field(context) {
            self.build_bool_query(context)
        } else {
            self.build_term_disjunction(context)
        }
    }
}

impl From<TermSetQuery> for QueryAst {
    fn from(term_set_query: TermSetQuery) -> Self {
        QueryAst::TermSet(term_set_query)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use tantivy::collector::TopDocs;
    use tantivy::schema::{FAST, INDEXED, STRING, Schema, TEXT};
    use tantivy::{Index, TantivyDocument};

    use super::TermSetQuery;
    use crate::query_ast::{BuildTantivyAst, BuildTantivyAstContext};

    fn term_set(terms: &[(&str, &[&str])]) -> TermSetQuery {
        TermSetQuery {
            terms_per_field: terms
                .iter()
                .map(|(field, values)| {
                    (
                        field.to_string(),
                        values.iter().map(|value| value.to_string()).collect(),
                    )
                })
                .collect(),
        }
    }

    fn assert_matches(
        schema: Schema,
        query: TermSetQuery,
        documents: &[serde_json::Value],
        expected_doc_ids: &[u32],
    ) {
        let mut index = Index::create_in_ram(schema.clone());
        index.set_tokenizers(
            crate::create_default_quickwit_tokenizer_manager()
                .tantivy_manager()
                .clone(),
        );
        let mut writer = index
            .writer_with_num_threads::<TantivyDocument>(1, 50_000_000)
            .unwrap();
        for document in documents {
            writer
                .add_document(TantivyDocument::parse_json(&schema, &document.to_string()).unwrap())
                .unwrap();
        }
        writer.commit().unwrap();
        let ast = query
            .build_tantivy_ast_call(&BuildTantivyAstContext::for_test(&schema))
            .unwrap();
        let query: Box<dyn crate::TantivyQuery> = ast.simplify().into();
        let searcher = index.reader().unwrap().searcher();
        let hits = searcher
            .search(
                &*query,
                &TopDocs::with_limit(documents.len() + 1).order_by_score(),
            )
            .unwrap();
        let mut doc_ids = Vec::new();
        for (score, address) in hits {
            assert_eq!(score, 1.0);
            doc_ids.push(address.doc_id);
        }
        doc_ids.sort_unstable();
        assert_eq!(doc_ids, expected_doc_ids);
    }

    #[test]
    fn test_term_set_regex_matches_literal_terms() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("text", STRING);
        let schema = schema_builder.build();
        let values = [
            "",
            "a.b",
            "x|y",
            r"[a]+(b)?{2}^$\",
            "été",
            "ABC",
            "line\nbreak",
        ];
        let query = term_set(&[("text", &values)]);
        let regexes = query
            .to_regexes(&BuildTantivyAstContext::for_test(&schema))
            .unwrap();
        assert_eq!(regexes.len(), 1);
        assert!(regexes[0].json_path.is_none());
        let documents: Vec<_> = values
            .iter()
            .chain(["axb", "x", "y", "abc", "ÉTÉ", "prefixa.b", "a.bsuffix"].iter())
            .map(|value| serde_json::json!({"text": value}))
            .collect();
        assert_matches(schema, query, &documents, &[0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_term_set_regex_keeps_term_query_case_sensitivity() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("text", TEXT);
        let schema = schema_builder.build();
        let documents = [serde_json::json!({"text": "HELLO"})];
        assert_matches(
            schema.clone(),
            term_set(&[("text", &["HELLO"])]),
            &documents,
            &[],
        );
        assert_matches(schema, term_set(&[("text", &["hello"])]), &documents, &[0]);
    }

    #[test]
    fn test_term_set_regex_mixed_fields_and_constant_score() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("text", STRING);
        schema_builder.add_text_field("other", STRING);
        schema_builder.add_u64_field("number", INDEXED);
        assert_matches(
            schema_builder.build(),
            term_set(&[
                ("text", &["a", "b"]),
                ("other", &["c"]),
                ("number", &["42"]),
            ]),
            &[
                serde_json::json!({"text": ["a", "b"], "other": "c", "number": 42}),
                serde_json::json!({"text": "no", "other": "c"}),
                serde_json::json!({"number": 42}),
                serde_json::json!({"text": "c", "other": "a", "number": 43}),
            ],
            &[0, 1, 2],
        );
    }

    #[test]
    fn test_term_set_regex_json_paths_and_types() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_json_field("_dynamic", STRING);
        assert_matches(
            schema_builder.build(),
            term_set(&[
                ("nested.text", &["42", "a.b", "true"]),
                ("other", &["keep"]),
            ]),
            &[
                serde_json::json!({"_dynamic": {"nested": {"text": "42"}}}),
                serde_json::json!({"_dynamic": {"nested": {"text": 42}}}),
                serde_json::json!({"_dynamic": {"nested": {"text": "a.b"}}}),
                serde_json::json!({"_dynamic": {"nested": {"text": true}}}),
                serde_json::json!({"_dynamic": {"other": "keep"}}),
                serde_json::json!({"_dynamic": {"nested": {"text": "axb"}}}),
                serde_json::json!({"_dynamic": {"other": "a.b"}}),
                serde_json::json!({"_dynamic": {"nested": {"text": {"child": "a.b"}}}}),
                serde_json::json!({"_dynamic": {"Nested": {"text": "a.b"}}}),
            ],
            &[0, 1, 2, 3, 4],
        );
    }

    #[test]
    fn test_term_set_regex_empty_sets_match_nothing() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("text", STRING);
        let schema = schema_builder.build();
        let documents = [
            serde_json::json!({"text": ""}),
            serde_json::json!({"text": "hello"}),
        ];
        assert_matches(schema.clone(), term_set(&[]), &documents, &[]);
        assert_matches(schema, term_set(&[("text", &[])]), &documents, &[]);
    }

    #[test]
    fn test_term_set_query_with_fast_only_field_returns_bool_query() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_u64_field("fast_field", FAST);
        let schema = schema_builder.build();

        let terms_per_field = BTreeMap::from([(
            "fast_field".to_string(),
            BTreeSet::from(["1".to_string(), "2".to_string()]),
        )]);
        let term_set_query = TermSetQuery { terms_per_field };

        let tantivy_query_ast = term_set_query
            .build_tantivy_ast_call(&BuildTantivyAstContext::for_test(&schema))
            .unwrap();

        let bool_query = tantivy_query_ast
            .as_bool_query()
            .expect("Expected BoolQuery for fast-only field, but got a different query type");
        assert_eq!(bool_query.should.len(), 2);
        assert_eq!(bool_query.must.len(), 0);
        assert_eq!(bool_query.must_not.len(), 0);
        assert_eq!(bool_query.filter.len(), 0);
    }

    #[test]
    fn test_term_set_query_with_indexed_field_uses_disjunction() {
        let mut schema_builder = Schema::builder();
        schema_builder.add_u64_field("indexed_field", FAST | INDEXED);
        let schema = schema_builder.build();

        let terms_per_field = BTreeMap::from([(
            "indexed_field".to_string(),
            BTreeSet::from(["1".to_string(), "2".to_string()]),
        )]);
        let term_set_query = TermSetQuery { terms_per_field };

        let tantivy_query_ast = term_set_query
            .build_tantivy_ast_call(&BuildTantivyAstContext::for_test(&schema))
            .unwrap();

        let leaf = tantivy_query_ast.as_leaf().unwrap();
        let debug_str = format!("{leaf:?}");
        assert!(debug_str.starts_with("Const(score=1, query=BooleanQuery"));
        assert!(!debug_str.contains("TermSetQuery"));
        let mut terms = BTreeSet::new();
        leaf.query_terms(&mut |term, needs_positions| {
            assert!(!needs_positions);
            terms.insert(term.clone());
        });
        assert_eq!(terms.len(), 2);
    }
}
