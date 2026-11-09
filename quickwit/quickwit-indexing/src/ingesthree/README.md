# Ingesthree S3 indexing

`S3Indexer::index_next` performs one assignment-to-publication pass:

1. Claim eligible files using `AssignFilesForIndexing`. An empty assignment returns `None`.
2. Load the index configuration and delete-opstamp snapshot for the group.
3. Download one input at a time. Check its registered length, index incarnation, framing,
   and checksums with `DocumentFileReader`.
4. Convert JSON documents, build splits, serialize, package, stage, and upload them.
5. Publish all outputs and complete all contributing inputs using `PublishFileSplits`.

The caller supplies the node/source labels, metastore, storage resolver, scratch directory,
shared split cache, upload limit, optional shared IO limiter, and optional fingerprinter.
With `ingest_api.enable_ingesthree` enabled, each indexer node starts
`quickwit_common::num_cpus()` workers, controlled by `QW_NUM_CPUS`. Without that override,
the existing CPU detection uses `KUBERNETES_LIMITS_CPU`, then the OS, then a default of 2.
The worker count must be positive. Each worker processes one assigned file at a time,
sharing these node resources and `INGEST_V2_SOURCE_ID`. Empty assignments back off from
100 ms to 5 seconds. Each worker restarts independently after errors, panics, or unexpected
exits, with a 1–30 second exponential backoff. Ingesthree indexers require `enable_standalone_compactors: true`. A Janitor
node hosts the standalone planner and separate Compactor nodes perform merges. The
default service startup continues to use v2.

## Reuse and execution

The existing v2 actors and this worker share `DocumentConverter`, split creation/insertion,
`IndexedSplitBuilder::finalize`, packaging, upload preparation, and `IndexingSplitStore`.
The split format, metadata, recovery footer, merge maturity, and local merge cache remain
unchanged. Index uploads share the existing node-wide upload semaphore.

There are no stage actors or stage channels. The async caller awaits each operation.
Mapper construction uses `run_cpu_intensive`. Reading, document conversion/insertion,
serialization, and packaging use the existing blocking runtime: writers and compression
can block beyond the small-task CPU pool's time budget. Storage/metastore calls remain async.
Running synchronous work retains the scratch root after caller cancellation and checks a
kill switch between documents and serialization steps. Synchronous work may finish its
current operation before releasing the directory.

Existing document routing and its overflow partition are preserved. Builders flush after
batches when their aggregate document or heap target is reached, and at group end. All
splits remain unpublished until every input has been processed. The mapper and delete
opstamp stay fixed across the group. Files contain JSON without source-format/transform
metadata, so source decoding and transforms belong before intake. Current mapping rules
apply at indexing time. Invalid documents are counted, logged, and skipped as in v2.
Corrupt files and operational failures fail the group. An all-invalid group completes
with zero outputs.

## Publication, compaction, and shutdown

Publication retries keep the same publication ID and input/output IDs. A definite loser
returns `published = false` with no published split metadata. Errors and cancelled
calls preserve remote objects. A new indexing attempt creates new output IDs. Failures
before publication leave inputs retry-eligible through the metastore's normal expiry rule.

The indexing worker ends at durable publication. The standalone compaction planner scans
published immature splits in the metastore every five seconds and assigns merges to
compactors. Its startup waits ten seconds for existing workers to report work, then waits
for all live indexers to advertise standalone mode. This includes v2 indexers during a
mixed-version rollout. Ingesthree sends no local merge notifications and retains no local
merge ownership or configuration-restart path.

After planner restart, discovery uses the same published split metadata, including outputs
whose publishing indexer has stopped. Compaction configuration caching and failed-worker
recovery follow the existing standalone implementation. Packaging resolves tag names and
types against each output split's actual schema, including when merging older mappings.

Before shutting down the actor universe, await
`indexing_service.ask(quickwit_indexing::ingesthree::StopS3Indexers).await`.
The message is idempotent. It cancels every worker's polling and current async indexing
attempt, joins all worker tasks, and suppresses restarts. Actor finalization also stops them.
Uploaded inputs and completion evidence remain intact. Unfinished assignments become eligible
after the metastore retry interval. Blocking operations retain their scratch root and observe cancellation between
operations.

Remote input/orphan cleanup and completion-evidence expiry remain separate work. The worker
never deletes remote inputs or outputs.

The publication API permits at most 1,000 outputs per group. Exceeding that limit fails the
group without completing any inputs. Retrying unchanged limits can reproduce the error,
even for a single input. Compatible intake/assignment/indexing limits or another explicit
handling policy are required before rollout. Batch byte limits and heap thresholds permit
overshoot, including a document larger than the batch target. Physical scratch-space,
indexing-memory, and worker-concurrency admission remain production prerequisites.

## Metrics

Initial publication uses the existing `quickwit_indexing_published_*` counters and
split-size histogram with `{index, source, merge_ops}` labels. Counters increment
once per output after a confirmed winning publication, outside the retry loop.
Losing attempts and unconfirmed responses contribute nothing. A process crash after
publication can still lose metric emission. Select `merge_ops="0"` to measure initial
indexing throughput without counting compaction rewrites.

Document conversion retains the existing processed-document and processed-byte
metrics. Those count processing attempts, including work that later fails or loses
publication. Writer-memory estimates use
`quickwit_memory_in_flight_data_bytes{component="index_writer"}` and stay with each
writer through serialization and cancellation.

Worker metrics use the `quickwit_indexing_ingesthree_` prefix:

| Suffix | Meaning |
| --- | --- |
| `configured_supervisors` | Supervisors owned by the indexing service, including restart backoff. |
| `live_supervisors` | Supervisor tasks from their first poll until destruction, including idle polling and backoff. |
| `active_assignments` | Nonempty assigned groups from metadata lookup through publication, including blocking work still exiting after cancellation. |
| `worker_failures_total{reason}` | Runs ending with `error`, `panic`, or `unexpected_exit`. |
| `worker_restarts_total` | Restarts begun after failure backoff completes. |

Shutdown cancellation increments neither failure nor restart counters. Active
assignments and writer memory can remain positive briefly after supervisor shutdown
while synchronous work exits. Classic actor-pipeline and mailbox-backpressure metrics
retain their existing meanings. Standalone compaction uses its existing metric families.

## Tests

The worker tests use the file-backed metastore and RAM object storage. They open uploaded
split bundles with Tantivy and check document counts, metadata, and atomic completion.
They cover shared and multiple outputs, all-invalid inputs, corrupt input after an earlier
upload, committed-but-lost publication replies, cancellation, competing workers, partition
overflow, clustering across batches, and heap flushing. A held synchronous operation checks
scratch ownership after cancellation. Reader tests reject wrong size/incarnation, damaged
checksums, oversized frame lengths, and truncation without modifying input.

Supervisor tests cover empty polling, idle/restart backoff, errors, panics, unexpected exits,
cancellation, and stop/drop behavior. Service tests use the file-backed metastore and RAM
storage to exercise opt-in, rejection of local-merge mode, CPU-based worker count,
publication without local merges, and idempotent shutdown. Both suites live in their implementation modules. Packager tests
cover tag extraction from older output schemas.

Running-server tests in `quickwit-integration-tests/src/tests/ingesthree_tests.rs` cover
worker restart and combine mixed-node HTTP/ES ingestion with actual standalone compaction.
The compaction helpers and planner-restart scenario live in `ingesthree_compaction_tests.rs`. They check
input deletion states, merged split identity, merge count, document counts, producer node,
and searchable contents. Restart coverage publishes while compaction is absent, stops the
indexer, then restarts the planner and compactor to discover and merge those durable outputs.

These tests do not establish live AWS, deployment, cleanup-race, or performance guarantees.
