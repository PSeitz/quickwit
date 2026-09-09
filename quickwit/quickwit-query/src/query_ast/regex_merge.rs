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
use std::convert::Infallible;

use regex_syntax::hir::Hir;

use super::{BoolQuery, BuildTantivyAstContext, QueryAst, QueryAstTransformer, RegexQuery};

/// Merge regex/wildcard alternatives by field within Boolean unions.
/// Call before predicate-cache injection, execution and warmup extraction.
/// Only resolves and parses patterns; compilation errors are left to the query builder.
/// May reorder clauses, emitting field groups in sorted order for deterministic cache keys.
///
/// The context does not tell us whether scoring is needed. Merging preserves matching, but
/// scores may change: a union automaton does not sum scores from overlapping alternatives.
pub fn merge_regexes(query_ast: QueryAst, context: &BuildTantivyAstContext) -> QueryAst {
    let Ok(transformed) = RegexMerger { context }.transform(query_ast);
    transformed.expect("regex merging never removes a node")
}

#[cfg(test)]
#[path = "regex_merge_tests.rs"]
mod tests;

struct RegexMerger<'a, 'ctx> {
    context: &'a BuildTantivyAstContext<'ctx>,
}

impl RegexMerger<'_, '_> {
    fn merge_union(&self, clauses: &mut Vec<QueryAst>) {
        if clauses.len() < 2 {
            return;
        }
        let mut groups: BTreeMap<String, Vec<RegexQuery>> = BTreeMap::new();
        let regex_clauses = clauses.extract_if(.., |clause| matches!(clause, QueryAst::Regex(_)));
        for clause in regex_clauses {
            let QueryAst::Regex(regex) = clause else {
                unreachable!("only regexes are extracted");
            };
            // Include the JSON path in the key: sharing a physical field alone is not enough.
            groups.entry(regex.field.clone()).or_default().push(regex);
        }
        for (field, group) in groups {
            if let Some(regex) = self.union_regex(&group) {
                clauses.push(
                    RegexQuery {
                        lenient: group.iter().all(|query| query.lenient),
                        ..RegexQuery::new(field, regex)
                    }
                    .into(),
                );
            } else {
                clauses.extend(group.into_iter().map(QueryAst::Regex));
            }
        }
    }

    fn union_regex(&self, clauses: &[RegexQuery]) -> Option<String> {
        if clauses.len() < 2 {
            return None;
        }
        let mut alternatives = Vec::with_capacity(clauses.len());
        for clause in clauses {
            let resolved = clause
                .to_resolved(self.context.schema, Some(self.context.tokenizer_manager))
                .ok()?;
            alternatives.push(regex_syntax::Parser::new().parse(&resolved.regex).ok()?);
        }

        // HIR resolves flags and comments; concatenating raw regex strings is unsafe.
        // Case behavior is already resolved, including wildcard normalization. The leading
        // flag prevents RegexQuery::to_resolved from applying automatic case folding again.
        Some(format!("(?-i){}", Hir::alternation(alternatives)))
    }
}

impl QueryAstTransformer for RegexMerger<'_, '_> {
    type Err = Infallible;

    fn transform_bool(&mut self, mut query: BoolQuery) -> Result<Option<QueryAst>, Infallible> {
        // Requiring multiple should clauses to match is not a union.
        if query.minimum_should_match.unwrap_or(0) <= 1 {
            self.merge_union(&mut query.should);
        }
        // Multiple exclusions mean NOT(union), regardless of minimum_should_match.
        self.merge_union(&mut query.must_not);
        self.transform_bool_children(query)
    }
}
