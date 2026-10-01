# Regex warmup and predicate caching

For non-scoring searches on single-segment splits, individual regex predicates
are wrapped in predicate-cache nodes. Cache hits skip automaton warmup entirely.
Regex alternatives remain separate AST nodes and cache entries; Boolean queries
combine their hits during execution rather than merging their automatons.

For misses, warmup batches the automatons by physical field and segment using
Tantivy's `warm_postings_automatons`. JSON-path prefixes remain part of each
automaton. Tantivy returns one document bitset per input automaton, in input
order, sharing dictionary traversal and postings decoding across the batch.

Quickwit converts these bitsets to compressed `HitSet`s on the search CPU pool
and stores them in the predicate cache. Keys use the split ID and serialized
query AST, exactly as cache-node lookups do. Multiple query spellings can resolve
to one automaton; warmup fills every corresponding key, including empty results.

Queries are built before warmup. A cache-miss scorer therefore checks the cache
again before evaluating its inner query, validates the segment ID, and applies
its boost to the cached hits. If the entry has been evicted or was not admitted,
the normal cache-miss path evaluates the query using the warmed postings.

Predicate-cache entries represent one segment and do not support scoring.
Scoring searches and multi-segment splits retain postings-only warmup rather
than computing bitsets they cannot reuse.
