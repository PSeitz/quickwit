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

    /// Resolve indexed string terms into literal regex alternatives, keeping other types unchanged.
    /// Returns empty lists for fast-only sets, which use the boolean query path instead.
    pub fn resolve_terms(
        &self,
        context: &BuildTantivyAstContext,
    ) -> Result<(Vec<ResolvedRegex>, Vec<Term>), InvalidQuery> {
        if self.has_fast_only_field(context) {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut text_terms = BTreeMap::new();
        let mut other_terms = Vec::new();
        for term in self.make_term_iterator(context)? {
            if term.typ() != Type::Str && term.value().json_path_type() != Some(Type::Str) {
                other_terms.push(term);
                continue;
            }
            // Match the full dictionary key, including the path and type for JSON terms.
            let text = std::str::from_utf8(term.serialized_value_bytes())
                .expect("string dictionary keys contain valid UTF-8");
            text_terms
                .entry(term.field())
                .or_insert_with(BTreeSet::new)
                .insert(regex::escape(text));
        }
        let regexes = text_terms
            .into_iter()
            .map(|(field, terms)| ResolvedRegex {
                field,
                json_path: None,
                // FST regexes match entire terms. Keep TermQuery's case-sensitive matching.
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
