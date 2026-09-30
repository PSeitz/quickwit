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

use super::QueryAst;
use super::cache_node::CacheState;

/// Cache coverage of a split's query, measured in atomic AST predicate occurrences.
/// Bool/boost/cache wrappers and MatchAll/MatchNone do not count as predicates.
/// A compound cache hit covers all its leaves, including nested cache nodes, once.
/// A term-set or full-text clause counts as one predicate, regardless of expansion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PredicateCacheStats {
    pub num_predicates: u64,
    pub num_cached_predicates: u64,
}

impl PredicateCacheStats {
    /// Inspect the AST after cache injection, before it is compiled to a Tantivy query.
    pub fn from_query_ast(ast: &QueryAst) -> Self {
        let mut stats = Self::default();
        stats.visit(ast, false, None);
        stats
    }

    /// Apply the same coverage rules using one virtual cache's lookup results.
    pub fn from_virtual_cache(ast: &QueryAst, cache_index: usize) -> Self {
        let mut stats = Self::default();
        stats.visit(ast, false, Some(cache_index));
        stats
    }

    fn visit(&mut self, ast: &QueryAst, cached: bool, virtual_cache: Option<usize>) {
        match ast {
            QueryAst::Bool(query) => {
                for child in query
                    .must
                    .iter()
                    .chain(&query.should)
                    .chain(&query.must_not)
                    .chain(&query.filter)
                {
                    self.visit(child, cached, virtual_cache);
                }
            }
            QueryAst::Boost { underlying, .. } => self.visit(underlying, cached, virtual_cache),
            QueryAst::Cache(node) => {
                let hit = match virtual_cache {
                    Some(index) if !matches!(node.state, CacheState::Uninitialized) => {
                        node.virtual_hits[index]
                    }
                    Some(_) => false,
                    None => matches!(node.state, CacheState::CacheHit(_)),
                };
                self.visit(&node.inner, cached || hit, virtual_cache);
            }
            QueryAst::MatchAll | QueryAst::MatchNone => {}
            QueryAst::Term(_)
            | QueryAst::TermSet(_)
            | QueryAst::FieldPresence(_)
            | QueryAst::FullText(_)
            | QueryAst::PhrasePrefix(_)
            | QueryAst::Range(_)
            | QueryAst::UserInput(_)
            | QueryAst::Wildcard(_)
            | QueryAst::Regex(_)
            | QueryAst::CalcField(_) => {
                self.num_predicates += 1;
                self.num_cached_predicates += u64::from(cached);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tantivy::index::SegmentId;

    use super::*;
    use crate::query_ast::{
        BoolQuery, CacheNode, HitSet, PredicateCache, PredicateCacheInjector, QueryAstTransformer,
        TermQuery,
    };

    struct TestCache {
        parent_key: String,
        child_key: String,
        real_parent_hit: bool,
    }

    impl PredicateCache for TestCache {
        fn get(&self, _split_id: String, query: String) -> Option<(SegmentId, HitSet)> {
            (self.real_parent_hit && query == self.parent_key)
                .then(|| (SegmentId::generate_random(), HitSet::empty()))
        }

        fn num_virtual_caches(&self) -> usize {
            3
        }

        fn get_with_virtual_hits(
            &self,
            split_id: String,
            query: String,
        ) -> (Option<(SegmentId, HitSet)>, Vec<bool>) {
            // First virtual cache has only the child; second has parent and child;
            // third misses everything, independently of the real cache.
            let hits = vec![
                query == self.child_key,
                query == self.parent_key || query == self.child_key,
                false,
            ];
            (self.get(split_id, query), hits)
        }

        fn put(&self, _: String, _: String, _: SegmentId, _: HitSet) {
            panic!("coverage measurement must not insert entries");
        }
    }

    #[test]
    fn test_virtual_coverage_is_independent_of_real_parent_hit() {
        let term = |value: &str| {
            QueryAst::from(TermQuery {
                field: "body".into(),
                value: value.into(),
            })
        };
        let child = term("first");
        let parent: QueryAst = BoolQuery {
            must: vec![
                CacheNode::new(child.clone()).into(),
                CacheNode::new(term("second")).into(),
            ],
            ..Default::default()
        }
        .into();
        let ast: QueryAst = BoolQuery {
            must: vec![CacheNode::new(parent.clone()).into()],
            must_not: vec![term("uncached")],
            should: vec![QueryAst::MatchAll, QueryAst::MatchNone],
            ..Default::default()
        }
        .into();
        for real_parent_hit in [false, true] {
            let cache = Arc::new(TestCache {
                parent_key: serde_json::to_string(&parent).unwrap(),
                child_key: serde_json::to_string(&child).unwrap(),
                real_parent_hit,
            });
            let injected = PredicateCacheInjector {
                cache,
                split_id: "split".into(),
            }
            .transform(ast.clone())
            .unwrap()
            .unwrap();
            assert_eq!(
                PredicateCacheStats::from_query_ast(&injected),
                PredicateCacheStats {
                    num_predicates: 3,
                    num_cached_predicates: if real_parent_hit { 2 } else { 0 },
                }
            );
            for (index, num_cached_predicates) in [1, 2, 0].into_iter().enumerate() {
                assert_eq!(
                    PredicateCacheStats::from_virtual_cache(&injected, index),
                    PredicateCacheStats {
                        num_predicates: 3,
                        num_cached_predicates,
                    }
                );
            }
        }
    }
}
