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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tantivy::Term;
use tantivy::query::{BooleanQuery, ConstScoreQuery};
use tantivy::schema::{FieldType, IndexRecordOption};

use crate::InvalidQuery;
use crate::query_ast::regex_query::json_str_term_prefix;
use crate::query_ast::{
    AutomatonQuery, BoolQuery, BuildTantivyAst, BuildTantivyAstContext, JsonPathPrefix, QueryAst,
    ResolvedRegex, TantivyQueryAst, TermQuery, get_or_compile_cached_fst_regex,
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

    fn try_string_set_regex(
        &self,
        terms: &BTreeSet<Term>,
        context: &BuildTantivyAstContext,
    ) -> Option<TantivyQueryAst> {
        let warmup = context.term_set_regexes?;
        if self.terms_per_field.len() != 1 || terms.is_empty() {
            return None;
        }
        let (full_path, _) = self.terms_per_field.first_key_value()?;
        let (field, entry, path) =
            super::utils::find_field_or_hit_dynamic(full_path, context.schema)?;
        if !entry.is_indexed() {
            return None;
        }
        let json_path = match entry.field_type() {
            FieldType::Str(_) => None,
            FieldType::JsonObject(options) => Some(json_str_term_prefix(field, path, options)),
            _ => return None,
        };
        // Ordered terms give the same compilation cache key on every split.
        let mut pattern = String::new();
        for (i, term) in terms.iter().enumerate() {
            // A JSON literal can also resolve to a typed term. Keep the exact disjunction
            // if any term does not belong to this path's string dictionary.
            let value = term
                .serialized_value_bytes()
                .strip_prefix(json_path.as_deref().unwrap_or_default())?;
            if i > 0 {
                pattern.push('|');
            }
            regex_syntax::escape_into(std::str::from_utf8(value).ok()?, &mut pattern);
        }
        let regex = get_or_compile_cached_fst_regex(&pattern)?;
        let query = AutomatonQuery {
            field,
            automaton: Arc::new(JsonPathPrefix {
                prefix: json_path.clone().unwrap_or_default(),
                automaton: regex,
            }),
        };
        warmup.borrow_mut().push(ResolvedRegex {
            field,
            json_path,
            regex: pattern,
        });
        // AutomatonWeight unions matching postings and assigns the boost once per document.
        Some(query.into())
    }

    fn build_term_disjunction(terms: BTreeSet<Term>) -> TantivyQueryAst {
        let term_queries = terms
            .into_iter()
            .map(|term| {
                Box::new(tantivy::query::TermQuery::new(
                    term,
                    IndexRecordOption::Basic,
                )) as Box<dyn crate::TantivyQuery>
            })
            .collect();
        // Score the union, not each term: matching several terms still contributes only 1.0.
        // Basic postings and query_terms also ensure warmup never requests positions.
        let disjunction = BooleanQuery::union(term_queries);
        ConstScoreQuery::new(Box::new(disjunction), 1.0).into()
    }

    fn resolve_terms(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<BTreeSet<Term>, InvalidQuery> {
        let mut terms = BTreeSet::new();

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
            return self.build_bool_query(context);
        }
        let terms = self.resolve_terms(context)?;
        if let Some(query) = self.try_string_set_regex(&terms, context) {
            return Ok(query);
        }
        Ok(Self::build_term_disjunction(terms))
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

    use tantivy::schema::{FAST, INDEXED, Schema};

    use super::TermSetQuery;
    use crate::query_ast::{BuildTantivyAst, BuildTantivyAstContext};

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
