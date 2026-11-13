# Term-set queries

`TermSetQuery` matches the union of its field/value pairs. Values use the same
schema resolution and raw-tokenizer handling as individual `TermQuery` clauses.

For indexed fields, query construction groups string terms by field and JSON
path. Each group becomes a full-term regex whose alternatives are escaped literal
values. Matching stays case-sensitive, including on fields with lowercasing
index tokenizers. The automatic case-insensitivity of user-supplied regex queries
is not applied to term sets.

JSON regexes use a byte prefix containing the encoded path and string type.
This keeps matching scoped to the requested path and value type. Numeric,
boolean, and other non-string terms keep their exact dictionary lookups. A JSON
value can produce both string and typed terms, and both remain in the union.

The indexed union has a constant score of 1.0, even when a document matches
multiple values or fields. Empty sets match no documents. If any targeted field
is fast-only, the entire set retains its existing boolean-query execution path.

Warmup uses the same resolved regex strings and JSON prefixes as query execution.
Remaining non-string terms are collected through `query_terms`. Both paths warm
basic postings without positions. Regex compilation errors propagate through the
usual query-validation handling.
