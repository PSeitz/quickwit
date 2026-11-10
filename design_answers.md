# ingesthree: user answers

## Current direction

The user proposed ingesthree, an ingestion design using per-index S3 input files, broadening the discussion beyond actor consolidation. This is the current design. The earlier actor-consolidation analysis and implementation sequence remain historical context. The user authorized a minimal implementation starting with the ingester. Implementation now includes the local-storage core, write queue, byte limits, document validation, automatic file finishing, and an active-writer limit, with an experimental opt-in service/routing connection. See [implementation_status.md](implementation_status.md) and the [local-core documentation](quickwit/quickwit-ingest/src/ingesthree/README.md).

**Ingesthree requires standalone compaction on separate compactor nodes.** Indexing ends at durable split publication. The existing compaction planner discovers published splits in the metastore and assigns merges to compactors. Indexer-local merge pipelines are outside the supported design.

**`WaitFor`, `Force`, and exactly-once semantics are not yet supported in this design.** The supported contract is at-least-once ingestion with acknowledgment after append and flush to the kernel. Acknowledgment does not wait for publication or guarantee search visibility. Support for these additional semantics is deferred.

```text
HTTP ingest → router forwards documents to an eligible ingester
same AZ first → index affinity → small self bonus among comparable destinations

Ingester, per index:
reserve write-queue space + local storage → validate → append locally
→ flush to kernel → acknowledge clients through the router
acknowledged data survives process/container crashes while host and storage remain available
size threshold or fixed timeout → finish file and start a new one
finished file → uploader → S3 → register available file in metastore

Independent indexing pipeline, one task with no stage actors:
self-assign eligible files for one index, oldest first → read inputs → process documents + index in RAM
→ finalize splits to disk + package → upload outputs
→ first publication atomically publishes outputs and completes all contributing files

Standalone compaction:
planner discovers published splits in metastore → separate compactor nodes merge them

Input cleanup after completion: Janitor periodically deletes completed inputs from S3
```

On decommission, stop receiving data and upload the remaining smaller files.

## Answers and clarifications

| Topic | User answer / proposal |
|---|---|
| Motivation | The user considers the current ingestion architecture **overengineered**. Remove **useless complexity** that has caused incidents. Other advantages are secondary. |
| Client durability | Confirmed: flush to the kernel before acknowledgment. Acknowledged data must survive a process/container crash while the host and storage remain available. All ingest clients share this policy without waiting for S3. Host-crash durability/fsync and pod-replacement storage remain separate open decisions. Permanent local-storage loss before S3 handoff remains outside the intended durability scope. Current ingestion's delayed `Flush` remains the comparison baseline for the existing implementation. |
| Client cancellation | Confirmed: cancellation before admission discards the request. After admission to the write queue, finish processing and, for valid input, append/flush despite caller disconnect or timeout. Validation/IO failures remain failures. Charge queued bytes until their buffers are released and local disk usage until safe file deletion. Client retries may duplicate data. |
| Client retries | At-least-once semantics. An ambiguous acknowledgment followed by a client retry may produce duplicate documents. Request-level deduplication is not being added. Recovery of the same file is a separate publication/registration concern. |
| Failure scope | Recover from crashes/failures at every step, including selective partitions, delayed requests, and lost responses. S3 service-outage handling remains deferred. |
| File contents | Each file accumulates data for one index across requests. Process and retry the whole file, without partial-file indexing checkpoints. One file may produce several output splits. A pipeline may combine several files for the same index into shared output splits. Publication and completion are atomic across all contributing files. A multi-index request can span independently published groups. |
| File limits | Accepted initial defaults: 100 MB (100,000,000 file bytes including header/framing), 10 seconds from file creation, and at most 1,024 active files. `QW_INGESTHREE_TARGET_UPLOAD_SIZE` overrides the per-index file target on each ingesting node when intake opens. It accepts byte counts or units such as `250MB` and `256MiB`; invalid or zero values fail startup. Finish files between batches, so a batch may exceed the size target. Incoming writes leave the deadline unchanged. At the active-file cap, reject requests needing an additional writer. Existing writers continue. The cap leaves existing files unchanged, and finishing files at size/time limits frees slots. Skip empty uploads and registrations. |
| Many-index case | Confirmed after comparing costs: retain single-index files for simplicity, accepting smaller files and more requests/jobs for low-volume indexes. Mixed-index files and per-index completion lists within a file are outside the chosen design. See the [cost example](ingest_comparison.md#single-index-file-cost-example). |
| File identity | Each file has a unique ID. Recovery preserves that ID. |
| File assignment | Pipelines self-assign in the metastore, selecting eligible files oldest first. Ready files and files processing beyond a retry threshold are eligible. Any pipeline can recover unfinished work. |
| Indexer concurrency | The user proposed `num_cpu` pipelines per indexer and emphasized that pipelines are independent. Reusable worker slots shared across indexes were recommended, but their exact scheduling/lifetime has not been confirmed. |
| Pipeline structure | One task performs document processing, indexing, serialization, packaging, upload, and publication. These are internal steps with no separate stage actors or actor mailboxes. This replaces the earlier open choice about remaining stage boundaries. |
| Pipeline supervision | One supervisor per indexer process observes pipeline failures and restarts failed or unexpectedly exited tasks with backoff. Restarts stop during drain or shutdown. Unfinished inputs remain recoverable through metastore eligibility rules. See [Pipeline supervision](#pipeline-supervision). |
| Compaction | Confirmed: support only separate compactor nodes. Reuse the standalone compaction planner's discovery of published splits. Remove ingesthree's indexer-local merge ownership, notifications, reconciliation, and configuration-restart path. See [Compaction](#compaction). |
| Indexing storage | Use `QW_ENABLE_IN_MEMORY_INDEXING` for index construction. Finalized splits on disk are accepted for packaging/upload. Fully disk-free indexing is outside the chosen requirements. |
| Duplicate registration | The metastore recognizes an already-indexed file and discards its repeated registration rather than indexing it again. Retain completion evidence. The retention sizing horizon is 30 days; safe retirement must also prevent expired inputs from being registered again. |
| Validation | Preserve existing configurable pre-acknowledgment validation and per-document errors. Faster validation is welcome. |
| Local storage implementation | Confirmed: separate per-index local append files, finished and uploaded unchanged as S3 inputs. Drop the shared `mrecordlog` layout, ingest shards, and mixed-index WAL segments. After finishing a file, the next append starts a fresh file with a new ID. Multiple ingesters may create files for the same index. The first implementation uses versioned, checksummed records and repairs only incomplete tails of open files. This experimental format is an implementation choice, not a released compatibility contract. Direct versus buffered IO is an implementation/performance choice, not a hard requirement. |
| Page cache | Bypassing the page cache is not a hard requirement. The user sees no advantage in caching the input backlog. Buffered versus direct IO remains an implementation/performance choice, including upload reads; no IO mode has been selected. |
| Startup recovery | Confirmed: finish recovered files and upload them to S3 under their original IDs before admitting new data. Prioritize saving recovered data to S3 over metastore reconciliation. Use fresh files for incoming data. This assumes surviving storage is available to the recovering process. |
| Ingest routing | Reuse the application router's payload forwarding and retry/result handling. Among eligible ingesters, prefer the same AZ, then continuing this index's existing file, with a small self bonus among comparable destinations. Index affinity is a preference, not exclusive ownership. |
| Backpressure | Admit only when both the write queue and local-storage budget have room, including reservations. Return HTTP 429 when routing cannot find capacity. Slow writes fill the queue. Slow uploads or registration retain local files even when the queue is empty. Preserve capacity to finish, recover, and drain accepted data. Queue size, storage budget, and reserve sizes remain open. |

Naming description: a pipeline claims eligible files for indexing and records their processing start in the metastore. The concept is **file assignment**, implemented by `AssignFilesForIndexing`.

## Future field-based partitioning

We may add partitioning by a document field, probably before indexing. A possible flow is to register uploaded files for partitioning first, then register the partitioned files for indexing. `RegisterFileForIndexing` names the indexing step explicitly. Partitioning placement and its registration, completion, and cleanup protocol remain future design work. No partitioning support is implemented.

## File assignment and publication

Pipelines initiate assignment. The metastore stores the state and checks conditional updates. It does not schedule work proactively.

1. Select the oldest eligible file: ready, or unfinished with a processing start older than the retry threshold. Its index determines the group. A pipeline may take additional eligible files for that same index, oldest first, capped by total input bytes and file count. Use available files without waiting for the group to fill.
2. Assign the selected files to the requesting pipeline and record their processing start only while they remain eligible. Checking eligibility and recording assignment are atomic. Concurrent pipelines select different eligible files.
3. The pipeline task processes whole files together, serializes and packages shared output splits, and uploads them. Publication is also performed by that task, without stage actors or actor-message handoffs. Crossing the retry threshold permits another attempt to start, while the earlier attempt remains allowed to publish. Retry groups may overlap earlier groups.
4. Lock all contributing input rows in a consistent ID order and check that none is completed. Atomically publish the full output set and complete all contributing files. **The first publication transaction to commit wins**, regardless of the latest recorded assignment. If any input is already completed, reject the entire group's publication. Successful publication makes the outputs available to the standalone compaction planner's metastore scan.
5. Losing attempts discard their own unpublished outputs. Cleanup must preserve the winner's outputs. Unfinished files from a rejected group remain recoverable through the normal eligibility rules.

Assignment limits redundant work. It grants no exclusive publication right. A slow attempt may win after another pipeline has started retrying the file. No assignment renewal or publication fencing by assignment identity is required by this design.

Recovery uses the same eligible-file discovery. It needs neither deterministic placement on the original pipeline nor node-level assignment bookkeeping or a local assignment journal. Rendezvous hashing is unnecessary for correctness. A separate recovery process is optional.

The retry threshold controls recovery delay versus duplicate work. One minute was illustrative. Recording completed processing durations in the metastore to tune the threshold was suggested, not decided. The implemented API orders by registration Unix seconds and file ID. Eligibility requires an unfinished file with no processing start or a start strictly before metastore time minus the requested positive retry delay. Worker defaults remain open.

`AssignFilesForIndexing` and `PublishFileSplits` are implemented in both metastore backends and through gRPC. Assignment accepts 1–1000 files and a positive byte target. The oldest file is included even when it alone exceeds that target, so oversized files cannot be stranded. Publication accepts up to 1000 outputs, including zero. A unique publication ID identifies each attempt's immutable input/output sets. Completion records retain the winning ID so retries reconcile lost responses even after output cleanup. The ID carries no assignment ownership or fencing. File indexing and standalone compaction are separate workers connected through published split metadata. Janitor deletes completed input objects separately from indexing. See the [API and persistence contracts](quickwit/quickwit-metastore/README.md#ingesthree-assignment-and-publication).

### Indexing scale-out

Add indexers to increase the number of independent pipelines. Each pipeline assigns itself eligible files through the metastore and reads them from S3. Any indexer can process those files, regardless of which ingester accepted the data. Scale-out requires no shard transfer or data rebalancing.

### PostgreSQL capacity and requirements

The user selected an upper design target of **1,000 pipelines**, assuming **10 MB/s per pipeline**, and **30-day retention**. At continuous full utilization this is **10 GB/s or 864 TB/day**, using decimal units. This is a sizing target to validate, not measured implementation throughput. For the calculations below, indexing throughput and input-file size use the same byte basis.

| Workload | Estimate at 100 MB average input size |
|---|---|
| Input registration | 100 files/sec, or 8.64 million/day |
| Retained input records over 30 days | 259.2 million |
| One-file assignment and publication | About 100 assignment + 100 publication transactions/sec |
| Baseline transaction total | About 300/sec including registration, before cleanup, retries, empty polls, split staging, merges, and other metastore work |
| Ten-file groups (1 GB) | About 100 seconds of indexing per group; 10 assignment + 10 publication transactions/sec, plus 100 registrations/sec |

Pipelines fetch and process inputs between assignments. They do not poll continuously while busy. Grouping reduces transaction frequency while the input-row processing rate stays around 100/sec. Smaller files finished at timeout increase registrations and retained row count at the same byte throughput: these estimates are a 100 MB workload, not an upper bound on metadata traffic.

Requirements and implementation recommendations:

- **Simple indexed assignment.** In a short transaction, select the oldest eligible file, then a capped set for that index, using `FOR UPDATE SKIP LOCKED`. Select a byte-limited prefix in application code from returned sizes. Avoid joins, aggregation, and window functions for group formation. Index ready and retry-eligible work for oldest-first/global and per-index selection. Use separate candidate queries if combining ready/expired predicates produces a poor plan.
- **Small active-work indexes.** Exclude completed history from assignment indexes, for example with partial indexes on state. Index input IDs for registration reconciliation and completion checks. Retained history need not fit entirely in RAM.
- **Short atomic publication.** Cap input count and output split count per transaction. Lock input rows in consistent ID order, validate all completion states, and publish/complete atomically. Release database connections before S3 reads, indexing, or output uploads. No assignment renewal is introduced.
- **Pooled connections and idle backoff.** Use a metastore connection pool rather than one connection per pipeline. Start load testing with an aggregate pool of a few dozen connections, adjusting for transaction latency and database capacity. Apply jittered backoff when no eligible work exists and test simultaneous pipeline startup.
- **Retention and maintenance.** Budget for 259.2 million input records at the stated average size, plus split metadata and indexes. Keep autovacuum/analyze ahead of state updates and expiry. Use batched expiry or time partitioning if measurements justify it. Expire completed records only after input cleanup and with a rule preventing old inputs from being registered again. Never expire unfinished work solely because the history horizon passed. Thirty-day data retention alone does not establish that late registration is impossible.
- **Durability and availability.** Keep PostgreSQL durable commits enabled for publication/completion. Provision backups/PITR and failover according to the service recovery objectives. Input acknowledgment's local flush policy does not weaken metastore transaction durability.

**Initial hardware recommendation for load testing, not a proven minimum:** one PostgreSQL writer with **8 vCPUs, 32 GB RAM, and 1 TB SSD-backed storage**, plus a standby if required by the availability target. At an illustrative **0.5–1 kB per retained input including its indexes**, 259.2 million records occupy about **130–259 GB**. Measure actual row/index sizes. The remaining capacity must cover split metadata, WAL, temporary space, bloat, and maintenance headroom; backups need their own capacity. Provision storage IOPS/throughput from measured WAL, checkpoint, vacuum, and query load rather than equating 300 transactions/sec with 300 IOPS.

Validate with a 30-day-sized history and representative active backlog: sustained registration/assignment/publication, one hot index versus many indexes, small files, overlapping retries, idle polling, expiry, and existing metastore traffic. Inspect `EXPLAIN (ANALYZE, BUFFERS)`, pool waits, lock waits, transaction tail latency, WAL/checkpoint load, and vacuum progress. The synthetic transaction model has now been [tested with the full 259.2-million-row input history](docs/internals/ingest_postgres_experiment.md), achieving 100 registrations/s and 99.956–100 completed files/s under 8-CPU / 32-GiB container limits. Real metastore integration, production retirement safety, long-term maintenance and generic storage sizing remain unvalidated.

### Input deletion: implemented

The user requested simple Janitor cleanup. Janitor deletes S3 inputs after atomic split publication and file completion. Successful split upload alone leaves the file unfinished and still needed for recovery. Cleanup may delete an input while a losing attempt is running because the winner's outputs are already published.

The existing garbage collector runs completed-input cleanup at startup and after each split-GC pass, with ten minutes between passes. `ListCompletedFiles` returns completed records in pages of at most 1,000, ordered by file ID with an exclusive cursor. Janitor deletes each recorded object through the storage resolver. Missing objects count as success. Individual failures are logged and leave other objects eligible for deletion. Listing failures stop that pass. The next pass retries from the beginning.

Completion records remain unchanged, preserving duplicate-registration and publication-retry evidence. Every pass revisits them: recovery can re-upload a completed input and crash before registration, so a permanent deletion marker would miss that copy. A crash during cleanup is safe because deletes are idempotent. Files completed behind the current cursor are collected on a later pass.

This intentionally simple implementation uses the existing GC actor and adds no cleanup state or configuration. Repeated scans and delete requests grow with retained completed history. Production-scale cleanup progress and safe completion-evidence expiry remain future work. Objects with no completed record, including unregistered uploads, abandoned outputs, and inputs whose records disappeared with index deletion, remain outside this cleanup. See [cleanup implementation](quickwit/quickwit-janitor/src/completed_file_cleanup.rs).

## Pipeline supervision

One task per indexer process owns the pipeline task handles, observes errors, panics, and unexpected exits, and starts replacements. This **pipeline supervisor** restores processing capacity with restart backoff to avoid crash loops. It stops restarting pipelines during drain or shutdown. Pipeline stages remain internal steps of one task, without stage actors or separate stage supervisors.

Awaiting task completion detects exits and panics without periodic polling. A Tokio `JoinSet` is an implementation option. Detecting a running but stuck pipeline additionally requires progress checks. Idle pipelines waiting for work are healthy. Restart-backoff parameters and any progress-check policy remain implementation choices.

The supervisor restores processing capacity. Metastore assignment rules recover unfinished inputs once they become retry-eligible, on any pipeline. Restarting a pipeline leaves input assignment and completion state unchanged. Atomic first-publication-wins remains the protection against competing attempts.

Supervision is lightweight coordination on the async runtime, separate from CPU-heavy indexing. Process-wide failures require the deployment system to restart the process. Existing `IndexingPipeline` supervises stage actors. Ingesthree retains supervision at the pipeline-task level. The assignment-to-publication worker pass is implemented in [`S3Indexer`](quickwit/quickwit-indexing/src/ingesthree/README.md). Opt-in service startup runs one worker per configured CPU (`QW_NUM_CPUS`), with file polling and independent restart supervision. Shutdown suppresses restarts. Unexpected intake closure fails the server for process-level recovery.

## Compaction

The user selected standalone compaction on separate compactor nodes to remove the local-merge integration. Ingesthree indexers require `enable_standalone_compactors: true` and reject startup without it. Router-only nodes need no compaction opt-in. Deploy the existing planner on a Janitor node and workers on Compactor nodes with standalone compaction enabled. All live indexers, including v2 indexers during migration, must advertise standalone mode before the planner starts processing.

The indexing worker's responsibility ends with `PublishFileSplits`. The planner periodically scans published immature splits in the metastore and assigns merges to compactors. After planner restart, the same durable metadata provides discovery, including when the publishing indexer has stopped. Ingesthree needs no local merge notification, ownership map, recovery scan, or merge-worker configuration restart. This supersedes the earlier `MergePlanner`/`NewSplits` handoff. Compaction configuration and worker recovery follow the existing standalone implementation.

## External sources

Sources use the common ingest interface, independently of the indexing pipeline. They receive the same acknowledgment after flush to the kernel as other clients and can then advance their own checkpoints. This covers process/container crashes while the host and storage remain available. Upload and registration proceed asynchronously. This supersedes the earlier proposal to acknowledge sources after S3 upload and registration.

The contract is **at-least-once**. Lost responses or a crash before saving the source checkpoint can cause replay and duplicate indexed documents. Source positions remain separate from whole-file indexing completion.

Exactly-once semantics are not yet supported. First-publication-wins prevents competing attempts from publishing the same input file more than once. It does not deduplicate documents accepted again through client or source retries.

## Startup recovery

Recovery completes the handoff of surviving accepted data before accepting more data. Finish recovered files regardless of size or age. For each file, upload under its original ID, register it, and delete its local copy. Intake stays closed until every recovered file completes these steps. Failures retain remaining local files. Subsequent intake uses fresh files. This supersedes the earlier upload-only startup gate: registration and local deletion now also block startup.

`ActiveFile::finish_at: Instant` stores the active writer's deadline for finishing the file, fixed at creation time plus 10 seconds. The deadline is kept in memory. The writer loop finishes files at their deadlines between requests. A batch already being validated or written can finish in its current file after expiry, then the loop finishes expired files before taking the next request. The append path leaves age checks to the writer loop. Recovery finishes old files immediately and starts with an empty active-writer map.

Recovery ordering:

1. Read each original file ID from its filename and its index incarnation from the header.
2. Validate records, repair only an incomplete final record in an open file, and finish every nonempty recovered input. Corruption or finished-file damage fails recovery.
3. Upload the next recovered input idempotently under its original ID.
4. Register or reconcile that ID with the metastore. A registered or completed record permits local cleanup. Completion evidence prevents a repeated upload/registration from creating new indexing work. Redundant S3 copies of completed inputs require cleanup.
5. Delete the local copy and release its disk charge. Retain local data until registration is confirmed. Retry failed deletion without repeating successful upload or registration.
6. Repeat for remaining recovered files, then open intake. An unavailable metastore blocks recovery. Cancellation retains the remaining files for the next restart.

Local validation, file finishing, upload/register/local-delete handoff, startup recovery, and full handoff drain are implemented. `WriteQueue::open` with an uploader waits for upload, registration, and local deletion of every recovered file before returning. `drain()` stops intake, finishes accepted writes, and waits for handoff and local deletion. Cancellation retains remaining files and leaves intake closed. Retryable failures keep their local files and charges. Registration reporting a missing index incarnation logs an error, deletes the uploaded object and local file, releases its disk charge, and continues. This applies during normal operation, startup, and drain. Other permanent registration failures stop the writer with data retained. Progress is in memory, so restart replays retained files under their original IDs. S3-emulator tests cover handoff and five process-kill windows around upload, registration, and local deletion. Live AWS and cleanup after index deletion following successful registration remain pending. Janitor's repeated completed-record scans cover late re-uploads while completion evidence remains. See the [handoff implementation and limits](quickwit/quickwit-ingest/src/ingesthree/README.md#upload-handoff).

The user selected per-index local append files that are finished and uploaded unchanged. This supersedes the earlier queue-per-file `mrecordlog` proposal. Recovering a file may repair an interrupted tail before finishing it. The resulting finished contents remain unchanged through upload/retry.

## Pod responsibilities, ingest routing, and disk sizing

Routing priorities, write-queue plus local-storage admission, and in-memory index construction with on-disk finalized splits are agreed. Running ingest and indexing together remains an option. Deployment topology, routing implementation, write batching, and resource sizes remain open. The assignment, retry, and acknowledgment decisions above remain unchanged.

### What runs today: code findings

- Quickwit service roles are configured per process. `Indexer` is a role, while ingester and ingest router are internal components rather than independently selectable roles: [service definitions](quickwit/quickwit-config/src/service.rs#L26).
- The ingest-v2 setup instantiates an ingest router on every node. It starts an ingester, with a WAL under `data_dir/wal`, on nodes with the indexer role: [startup](quickwit/quickwit-serve/src/lib.rs#L1123). Thus an indexer process currently combines ingest persistence and indexing. This describes application composition, rather than a particular Kubernetes deployment.
- The current router obtains routing information from the control plane, selects an ingester for each index/source subrequest, groups subrequests by destination, and forwards the actual document payload in persist RPCs. The client keeps its HTTP request open and receives the result through the router. There is no HTTP redirect: [router](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L351).
- Current selection prefers the same AZ, then the higher free-capacity score among two sampled candidates, with open-shard count as a tiebreaker. It gives no extra preference to itself: [routing table](quickwit/quickwit-ingest/src/ingest_v2/routing_table.rs#L86). Selecting the same process bypasses the network stack: [client setup](quickwit/quickwit-serve/src/lib.rs#L1281).
- Current capacity scoring uses WAL disk and memory headroom and their recent change: [capacity tracker](quickwit/quickwit-ingest/src/ingest_v2/wal_capacity_tracker.rs#L72). This is distinct from how quickly one index's file fills.

An ingest router chooses where incoming data is accepted. File assignment chooses which indexer processes an input already registered in S3. They are separate decisions. S3 removes the requirement for the indexing worker to run on the pod that accepted the data.

### Deployment options: proposals

| Pod composition | Benefit | Cost |
|---|---|---|
| Ingester + local storage + S3 uploader + indexing workers | Fewer deployment types and a simple initial rollout. Any worker can still process any eligible S3 input. | Ingest validation, upload, and indexing compete for CPU, memory, disk, and network. Scaling changes both ingest and indexing capacity. |
| Ingest pods with local storage/upload; separate indexing pods | Scale and provision the two workloads independently. Indexing pressure is isolated from direct-client acknowledgment latency. | More deployment configuration and service discovery. Separate roles require implementation changes in this checkout. |

Recommendation for discussion: start with combined pods if operational simplicity is the priority. Reserve resources for intake and upload so indexing cannot prevent draining local data. Keep the S3 handoff independent of placement so separation remains possible. This is a recommendation, not a deployment decision.

In either layout, the metastore and S3 remain shared services. Separate compactor nodes perform merges. Placement of external-source readers remains open.

### Routing under ingesthree: agreed

Reuse the application ingest router's payload forwarding, destination clients, and retry/result handling. Adapt destination selection to file ingestion and remove open-shard discovery from this path. This supersedes the earlier recommendation to start with HTTP load balancing alone.

Overload protection and input-file sizing are separate concerns. Eligible ingesters must be ready, accepting intake, and able to admit the request. Among those destinations:

1. Prefer the same AZ. Cross-AZ routing remains available when no same-AZ destination can accept work.
2. Prefer continuing this index's existing file rather than spreading each batch across more writers.
3. Give a small bonus to the router's own ingester among comparable destinations.

Index affinity is a preference, not exclusive ownership. Multiple ingesters may create uniquely identified files for the same index, including when a destination is overloaded, unavailable, or draining. Routers with different membership views may select different writers. Routing around a failed pod sends new requests elsewhere. Recovery of its acknowledged local data still requires access to surviving storage. The service uses ready-node gossip capability and always tries its own node first when it advertises positive capacity. Peer selection samples two same-AZ candidates (or other AZs when exhausted), preferring higher capacity and then rendezvous affinity. Up to three distinct capacity/unavailability attempts include the initial local attempt. Intake advertises a node-wide capacity score immediately and every five seconds, reusing the v2 queue/disk headroom-and-trend calculation with ingesthree counters, including disk reservations. Routers only try positive-capacity destinations, with local admission rechecking available space. Skipped nodes consume no attempts. At or below 5% remaining in either budget, or with closed intake, the score is zero. Missing feedback is ineligible until advertised. All known destinations saturated returns HTTP 429. Saturation observed through gossip or rejection remains the failure reason if the other destinations are unavailable. Local admission remains authoritative for stale feedback and concurrent requests. File-presence hints remain pending. There is no ownership lease.

The file-filling target is 100 MB in under 10 seconds where traffic permits. This needs over 10 MB/s for that index on each writer, measured in the same bytes as the file-size threshold. A busy node processing other indexes offers no file-filling advantage for this index. Same-AZ priority can create writers in multiple AZs. Low-volume indexes still produce smaller files at timeout. The initial implementation uses 100,000,000 uncompressed file bytes including framing, a 10-second creation deadline, and a 1,024-active-file cap. Validation or IO already in progress can delay timer-driven file finishing.

### Transition from ingest-v2: agreed

Keep the two routing paths and their destinations separate. The ingesthree router targets only ingesthree ingesters, without v2 shard discovery or creation. V2 keeps its existing router and ingesters. No per-index ingestion-version setting, version negotiation, or cross-version fallback is part of the transition.

The ingesthree router sends `ingest-version: ingesthree` in the router-to-ingester header/RPC metadata. Ingesthree rejects a missing or incorrect header before admitting or writing any documents. This is a compatibility guard, rather than a replacement for keeping destination sets separate. An explicit gossip capability separates the destination sets. `ingest_api.enable_ingesthree: true` opts router and indexer nodes into this path. The default remains v2.

Move incoming traffic to ingesthree routers. Once v2 receives no new writes and its outstanding requests have settled, let its existing indexing path finish and reclaim the accepted backlog before retiring it. Accepted v2 data stays on the v2 path rather than being converted or replayed into ingesthree.

Implementation starts with the ingester. Production rollout requires the additive metastore schema/service changes and ingesthree indexing workers before traffic switches. The published split format stays unchanged. Merging uses the existing standalone compaction system, with no indexer-local merge pipeline. Input paths default from existing settings rather than introducing required configuration. The local core uses `<data_dir>/ingesthree`; uploaded input uses `<default_index_root_uri>/.ingesthree/<file_id>`, which must be readable by all ingesthree indexers. The service path supports Auto commits and a JSON ingest source without a transform. Other commit modes and legacy ingest are rejected.

### Ingester admission: agreed

Admit a request only when both the write queue and the local-storage budget have room, including reservations. Reject at the destination when either cannot accommodate intake. The router can try another destination and returns HTTP 429 when it cannot find capacity. Router observations are advisory. The destination enforces admission against concurrent requests.

The write queue applies backpressure when append/flush work falls behind. The local-storage budget covers active and finished input plus reservations until confirmed upload and registration allow local deletion. Slow uploads or registration can exhaust this budget while the write queue remains empty. Keep capacity reserved to finish, recover, and drain accepted data. Queue capacity returns when buffered write work is released. Disk capacity returns after safe local deletion.

An empty write queue is healthy. File affinity supplies the concentration benefit. There is no chosen target for general node utilization or disk fullness. Queue size, budget, reserve sizes, and router feedback details remain open.

### Indexing storage: agreed

Use `QW_ENABLE_IN_MEMORY_INDEXING` for index construction. Finalized splits on disk for packaging and upload are accepted. This matches the existing RAM-directory path, which persists to the split directory at finalization: [builder](quickwit/quickwit-indexing/src/models/indexed_split.rs#L99), [finalization](quickwit/quickwit-indexing/src/models/indexed_split.rs#L172). Fully disk-free finalization, packaging, and upload are outside the chosen requirements.

Limit pipeline concurrency by memory and CPU resources, and account for finalized splits and upload traffic. If indexing shares an ingest volume, its temporary outputs must leave the ingester's storage budget and drain reserve available. Exact resource limits and cleanup mechanics remain open. Compression is an option, not a selected format.

### Batch writes and durability: proposal

Batching can reduce write and flush overhead. Accumulate records for a short maximum delay or until a byte threshold, then append together and flush to the kernel before acknowledging the covered requests. Grouping requests under one persistence operation is commonly called group commit. Fsync remains an option for stronger host-crash durability. Batch memory must count toward admission. A failed append or flush cannot satisfy the acknowledgment contract.

The local write-batch timer is separate from the timer for finishing input files. Millisecond-scale write batches can accumulate into much larger S3 inputs. Write-batch sizes and timings remain open. External sources use the same local acknowledgment policy as other clients.

The physical layout matters:

- The rejected shared-WAL alternative can synchronize records for many indexes together, but single-index S3 inputs then need extraction.
- With separate per-index local files, batching reduces synchronization frequency within each file. A single file fsync does not cover other files. Many active indexes can still cause many small writes and syncs.
- A per-index append file can contain many batches and becomes one S3 input after it is finished. It is neither one local file per request nor one permanent file per index.

For scale only, a continuously busy single shared log batching every 10 ms would have roughly 100 group synchronizations per second. That is not a device IOPS estimate. Separate active files, metadata writes, filesystem behavior, and the storage device change the actual I/O count and latency. No batch interval has been selected.

### Disk requirements to measure

Disk selection needs a workload and acknowledgment-latency target. Raw IOPS alone is insufficient.

| Requirement | Inputs and implications |
|---|---|
| Sustained bandwidth | Accepted on-disk bytes/sec, concurrent upload reads, and recovery reads. A direct-file implementation normally writes input locally and reads it again for upload. Shared-WAL extraction may add copying or temporary I/O. Co-located indexing adds its own workload. |
| Persistence latency and operation rate | Request size/rate, number of active files, batch byte limit, maximum batch delay, and the chosen flush/sync policy under concurrent reads/writes. Measure acknowledgment tail latency, including queueing and validation. Fsync-before-acknowledgment is an optional comparison, rather than an assumed extra cost. |
| Capacity | Peak local backlog awaiting upload/registration, active files, reserve for recovery and finishing, and any indexing scratch space. A first approximation is input rate multiplied by the intended backlog time. Under slower uploads, backlog grows at intake rate minus successful drain rate. |
| Many-index overhead | Active file descriptors, per-file buffers, small writes/syncs, metadata for finished files, and S3 small-object count. Include quiet indexes and hot-index concentration. |
| Volume lifetime | A container/pod restart must be able to recover surviving accepted data. Specify whether storage survives pod replacement or requires reattaching a volume. Permanent local-storage loss before S3 handoff remains outside the durability contract for all ingest clients. |

Illustrative capacity only: 100 MiB/s of on-disk intake with no local cleanup for ten minutes accumulates about 59 GiB, before reserve and indexing scratch space. This does not select an outage-handling policy. S3 service-outage handling remains deferred.

Benchmark candidate storage with the actual append/sync sizes, upload reads, active-index count, and co-located indexing load. Check sustained drain rate and latency under pressure before choosing provisioned IOPS, throughput, or volume size. No disk benchmark has been run.

## Storage findings and trade-offs

These are code findings and technical constraints, not additional user decisions.

### Current ingest-v2 and `mrecordlog`

- This checkout has already removed ingest replication. `replication_factor` and its environment variable are ignored: [configuration](quickwit/quickwit-config/src/node_config/mod.rs#L711).
- Ingest-v2 identifies logical queues by `(index UID, source ID, shard ID)`. An indexing pipeline belongs to one index/source and fetches its assigned queues.
- The pinned `mrecordlog` revision (`3b3562ef`) multiplexes queues into shared physical WAL files. A logical queue belongs to one index, but its backing file can contain several indexes. Single-index upload files would require extraction.
- It retains untruncated records in per-shard in-memory queues. Fetch reads those queues through `range`. Startup replays shared disk WAL files to rebuild them. Truncation frees queue memory, while shared files are reclaimed once none of their records are still needed. Buffered I/O can also populate the kernel page cache.
- It provides queue creation, listing, range reads and deletion. Opening the WAL replays records to reconstruct the in-memory queues. Queue creation and deletion explicitly fsync.
- The inspected recovery loop logs and skips some corrupted records. Its handling of interrupted writes versus unexpected corruption needs validation before adopting it unchanged.
- Current ingest-v2 opens the log with a five-second delayed `Flush` policy. This remains the existing-implementation baseline. ingesthree requires flush before acknowledgment.

### Flush, fsync, and pod crashes

The pinned `mrecordlog` uses a userspace `BufWriter`. `PersistAction::Flush` flushes it to the operating system. `FlushAndFsync` also calls `sync_data` and synchronizes the directory. These are code findings in the dependency's `src/rolling/directory.rs`.

- Data still in the process's buffer can be lost on an abrupt process/container crash, even when the host survives.
- A successful buffered-file flush moves data to the kernel. Under normal local-filesystem behavior, a process crash leaves that page-cache data intact while the host/kernel continues running. Recovery also requires the file and volume to remain accessible.
- Pod replacement can remove a container writable layer or `emptyDir` volume even on a surviving host. Volume lifetime and remount/recovery behavior must be specified separately from the flush policy.
- Fsync adds persistence protection against host/kernel failure within the filesystem/device guarantees. It does not preserve a volume that is deleted or permanently lost.

Current `OnDelay` checks the elapsed interval during log operations. It is not an independent timer guaranteeing a flush of idle buffered data every five seconds. An acknowledgment under the current policy can precede flush, so keeping that policy does not guarantee survival of every acknowledged record after a pod/process crash.

The user confirmed flush-before-acknowledgment for the proposed design: acknowledged records must survive a process/container crash while the host and storage remain available. This closes the userspace-buffer loss window without requiring fsync. Host-crash durability and storage survival across pod replacement remain open.

At-least-once retries cover ambiguous or failed requests. They cannot recover acknowledged records lost after a producer advances its checkpoint. Validate the selected contract with the storage implementation and actual volume type, including interrupted-write recovery. No crash test has been run.

Inspected code: `quickwit/quickwit-ingest/src/ingest_v2/{state,fetch}.rs`, `quickwit/quickwit-ingest/src/mrecordlog_async.rs`, and the pinned dependency's `src/multi_record_log.rs`, `src/frame/reader.rs`, and `src/rolling/directory.rs`. These findings come from code inspection. No crash tests or performance measurements have been run.

### Per-index files and direct I/O

The selected per-index append-only file serves as both local recovery log and S3 input, avoiding queue extraction and the need for full-backlog retention in application memory. It still needs reliable framing, corruption detection, synchronization and recovery. The library/format choice remains open.

Ordinary file writes can leave an incomplete tail after a crash. If fsync-before-acknowledgment is selected, successful fsync protects acknowledged data under the filesystem's durability guarantees. Complete data whose acknowledgment was interrupted may also survive, consistent with at-least-once client semantics. Recovery must distinguish an interrupted append from unexpected corruption rather than truncate indiscriminately. Recovery guarantees for later acknowledgments after an earlier failed append must match the selected persistence boundary.

`O_DIRECT` bypasses the OS data page cache. It does not alone guarantee persistence through volatile device caches or make interrupted writes atomic. Discussed durability options were direct writes followed by `fdatasync`/`fsync`, or `O_DIRECT | O_DSYNC`. File creation and rename durability also require directory synchronization. Direct I/O requires suitable buffer, offset and length alignment, including treatment of the final partial block. Buffered upload reads would populate the page cache again.

Admission must preserve a path to finish, recover, and drain accepted files when intake is stopped. Reserve sizes and the file-finishing mechanism can be chosen later.

## Validation and mapping changes

The user expects incompatible data to fail. Current ingestion validates against the shard's mapper before persistence when enabled: [ingester.rs](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L518).

Mapping changes replace affected indexing pipelines. New pipelines build their mapper from the updated configuration. During indexing, invalid documents are logged, counted, and skipped while source progress advances: [scheduling](quickwit/quickwit-control-plane/src/indexing_scheduler/scheduling/mod.rs#L278), [mapper construction](quickwit/quickwit-indexing/src/actors/indexing_service.rs#L342), [document processing](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L467).

The user accepts work using old or new mappings around an update and does not want mapping-version consistency treated as a design concern. Keep existing mapping behavior; acceptance-time mapping pinning is outside the chosen design. Failing the entire input file has not been selected.

## Current interview status

Per-index append files uploaded unchanged, file-finishing semantics, source delivery semantics, routing priorities, write-queue plus local-storage admission, in-memory index construction with on-disk finalized splits, and the assignment/publication rules above are agreed. Remaining choices include:

- Validate the agreed flush-before-acknowledgment contract for process/container crashes with host and storage available. Decide host-crash durability/fsync and storage survival across pod replacement separately.
- Ingest/indexing pod composition, placement of external-source readers, routing implementation, local write batching, and disk provisioning. Separate compactor nodes are required. Set write-queue size, local-storage budget, drain reserve, and indexing memory/scratch limits. See the agreed routing and admission rules above.
- Worker defaults for the indexing retry delay, group limits, and pipeline count. The metastore APIs define eligibility and ordering. Local file limits use the accepted 100 MB / 10-second / 1,024-active-file defaults. Processing-duration tracking is a tuning option.
- Storage format, interrupted-write recovery, ambiguous handoff/publication reconciliation, and cleanup mechanics. Completed-input deletion by Janitor is implemented. Attempt outputs must remain distinguishable so losing attempts cannot damage the winner.
- Index-deletion handling and actionable outcomes for permanently unprocessable files. Mapping-version consistency is outside the chosen requirements.

## Corrections to the previous review

The earlier duplicate-indexing example assumed completed metastore records were deleted before an old producer retried registration. That assumption was not part of the user's proposal. Retained completion evidence supports the user's **“already indexed → discard”** rule.

At the time of the earlier review, no background garbage collector had been proposed. The user has since requested a simple collector for completed input files; Janitor implements the arrangement above. The finding about Quickwit's existing output GC belongs to a compatibility check if that code is reused. It does not establish a defect in ingesthree.

Janitor revisits completed records to delete redundant S3 copies, including copies re-uploaded before a crash preceding registration. Uploads without retained completed records still need separate cleanup. An unused S3 copy is distinct from duplicate indexed data.

## How to continue

- Compare the approaches in [ingest_comparison.md](ingest_comparison.md). Use [validation.md](validation.md) to identify incorrect or underdefined concepts. Leave implementation details for later where the concept is sufficient.
- Ask one consequential design question at a time, with a recommendation, and wait for feedback. Investigate factual implementation behavior in code.
- Keep user decisions separate from implementation recommendations. Writing down a proposal does not make it accepted.
- Minimal ingester implementation and focused local-core tests are authorized and started. Full distributed failure verification and performance experiments remain separate work. Keep the implementation out of production routing until the supporting metastore and indexing path is ready.
