# Predicate cache metrics

Predicate cache effectiveness is measured once per **split search**, not per cache
lookup. A cache entry can replace one predicate or a compound query, so an entry
hit ratio does not measure how much query evaluation the cache avoids.

- `quickwit_search_predicate_cache_split_searches_total{outcome}` counts split
  searches with `hit` (at least one predicate served from a cached query subtree,
  or a cached absent required term prunes the split) or `miss`.
- `quickwit_search_predicate_cache_predicates_total{status}` counts atomic AST
  predicate occurrences, with `status="cached"` or `status="uncached"`.
- `quickwit_search_virtual_predicate_cache_split_searches_total{capacity,policy,outcome}`
  and `quickwit_search_virtual_predicate_cache_predicates_total{capacity,policy,status}`
  apply the same rules independently to each configured virtual cache. `capacity`
  is the resolved capacity in bytes; `policy` is the resolved eviction policy.

A hit on a cached `A AND B` covers two predicates, not one entry. Boolean, boost,
and cache wrappers do not count as predicates; neither do `MatchAll` and
`MatchNone`. Each other AST leaf counts once, including a term-set, full-text, or
calculated-field clause regardless of its internal expansion. Repeated predicates
count as separate occurrences. Nested cache hits never double-count coverage.
Cache injection still looks up children below a cached parent so virtual caches
observe those accesses. A real-cache parent hit does not imply a virtual-cache hit:
if the virtual cache misses the parent but hits one child, only that child's
predicates are covered. A cached absent required term covers the whole query
because it avoids all predicate evaluation. Every required term is checked so
one cache's absence hit cannot hide another cache's hit on a different term.
With virtual caches configured, required terms are also collected before real-cache
substitution so cached subtrees cannot hide virtual term-absence hits.

The denominator includes uncached leaves outside cache nodes (for example a time
range). These are structural coverage metrics, not estimates of CPU time saved.
Measurements use the split-normalized AST after cache injection. Split searches
served by the partial-result cache or pruned before query construction are not
included. Disabled predicate caches and queries without atomic predicates are
excluded. Scored queries cannot reuse cached query subtrees, but can still hit
the term-absence cache and are included when the predicate cache is enabled.

Example PromQL, aggregated across searchers:

```promql
# Fraction of split searches benefiting from any predicate cache entry.
sum(rate(quickwit_search_predicate_cache_split_searches_total{outcome="hit"}[5m]))
/
sum(rate(quickwit_search_predicate_cache_split_searches_total[5m]))

# Fraction of atomic predicates covered (weighted by predicate count).
sum(rate(quickwit_search_predicate_cache_predicates_total{status="cached"}[5m]))
/
sum(rate(quickwit_search_predicate_cache_predicates_total[5m]))
```

The generic cache hit, hit-byte, and miss metrics with `component_name="predicate"`
are no longer emitted for the real cache. They mixed single-predicate,
compound-predicate, and term-absence lookups. Cache size and eviction metrics are
unchanged. Virtual caches retain their lookup hit, hit-byte, and miss metrics for
comparing capacities and policies, but use the new virtual coverage metrics for
comparisons with real-cache coverage. For example:

```promql
sum by (capacity, policy) (
  rate(quickwit_search_virtual_predicate_cache_predicates_total{status="cached"}[5m])
)
/
sum by (capacity, policy) (
  rate(quickwit_search_virtual_predicate_cache_predicates_total[5m])
)
```

Virtual caches still replay the real cache's lookup and insertion stream, storing
only keys and sizes. Coverage is calculated independently from each cache's hits,
but this is not a full counterfactual execution: virtual misses do not execute
queries or create entries that the real execution never produces. For example,
a real parent hit prevents evaluating and filling missing children in all caches.
A disabled real predicate cache also disables its virtual simulations.
