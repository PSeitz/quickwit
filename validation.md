# ingesthree: correctness validation

Find incorrect or underdefined concepts through action-by-action failure traces. Leave API layouts, thresholds, and other implementation details for later.

**Status:** parallel conceptual reviews and code inspection completed. Local storage and a write queue are implemented, with 40 focused tests passing, including process-exit recovery, incomplete-tail recovery, concurrent queue/disk admission, validation, size/fixed-time file finishing, active-writer limits, and drain. Full action-by-action coverage remains pending. The broader findings below are analytical rather than reproduced failures. See [implementation_status.md](implementation_status.md) for exact scope and commands. No end-to-end handoff/publication tests or benchmarks have run.

## Baseline

The decision log is [design_answers.md](design_answers.md).

- All ingest clients receive acknowledgment after append and flush to the kernel. Acknowledged input must survive process/container crashes while the host and storage remain available. Host-crash durability/fsync and pod-replacement storage remain open. Client retries are at-least-once.
- Reuse router payload forwarding and retry/result handling. Among eligible destinations, prefer the same AZ, then continuing this index's file, with a small self bonus among comparable candidates. Affinity is advisory. Multiple writers per index remain valid.
- Keep v2 and ingesthree router/ingester paths and destinations separate. The ingesthree router sends `ingest-version: ingesthree`; ingesthree rejects missing or incorrect headers before admission or writes. Test both rejection cases, compatible intake, destination separation through retries, and absence of v2 shard discovery/creation on the ingesthree path. Migration moves incoming traffic while v2 finishes its accepted backlog before retirement. No per-index version setting, negotiation, or cross-version fallback.
- Admission requires room in both the write queue and the local-storage budget, including reservations. Reject when either cannot accommodate intake. At the open-file cap, reject requests needing another writer while allowing existing writers to continue. Return HTTP 429 when routing cannot find capacity. Preserve room to finish, recover, and drain accepted input. File-filling optimization is separate from overload protection.
- Caller cancellation before admission discards work. After write-queue admission, work continues despite disconnect/timeout, subject to validation/IO errors. Queued bytes remain charged until buffers release, disk usage until safe local deletion. Test cancellation on both sides of admission and verify surviving work retains its budget.
- External producers use the common ingest interface and advance their own checkpoints after acknowledgment. Upload and registration proceed asynchronously.
- Each local append file accumulates data for one index across requests, keeps its ID across retries, and uploads unchanged after finishing. The layout has no ingest shards or shared mixed-index WAL segments. Process and retry the whole file. Recovery finishes old files and starts fresh ones. Multi-index requests can span independently published files. Mixed-index files and per-index completion lists within a file are outside the chosen design.
- Each independent indexing pipeline is one task covering input reading, document processing, indexing, serialization, packaging, upload, and publication. There are no separate stage actors or actor mailboxes. Use `QW_ENABLE_IN_MEMORY_INDEXING` for construction. Finalized splits on disk for packaging/upload are accepted. Budget RAM and scratch space, preserving ingest capacity when co-located.
- Independent pipelines self-assign files through conditional metastore updates, oldest first among ready files and files processing beyond the retry threshold. Recovery can use any pipeline, without a node-level assignment map or journal.
- **First publication wins:** atomically publish the full output set and complete every contributing file in a same-index group. Reject the entire publication if any input is already completed. Retry groups may overlap. Earlier attempts remain allowed to publish after reassignment. Losing attempts discard only their own outputs, and unfinished inputs from rejected groups remain recoverable.
- **Already indexed → discard:** metastore completion evidence prevents repeated registration from creating new indexing work. Physical deletion of redundant uploads remains a cleanup detail.
- The user suggested a Janitor task for completed inputs. The latest proposal gives Janitor sole responsibility for completed-input deletion, pending confirmation. Existing output-cleanup code is a separate optional compatibility concern.
- Incompatible documents are expected to fail. Today indexing logs/counts and skips them while advancing progress. Keep existing mapping behavior; old/new mappings around updates are accepted and acceptance-time mapping pinning is outside scope. Whole-file failure has not been selected.
- Retry timing and the oldest-first timestamp remain open. Recording processing durations for tuning is a suggestion. The design uses no assignment renewals or exclusive publication rights.

## Model

Model these components independently so overlapping operations remain visible:

| Component | Facts to represent |
|---|---|
| Router | Membership/availability views, AZ, index affinity, self preference, forwarded document payloads, outstanding attempts, and observed results/rejections. |
| Ingester/filesystem | File identity, active/finished contents, write-queue occupancy, userspace buffers, kernel page cache, synchronized data/metadata, volume lifetime, local-storage budget, reservations, drain reserve, and local deletion. |
| Metastore | File absent, available, processing, or completed with its winning outputs. Assignment/start information, ordering timestamp, conditional updates, and transaction boundaries. |
| Each pipeline | Its processing attempt, input processed, verified end of input, RAM builder state, finalized split files, output objects, resource accounting, and outstanding requests. Multiple attempts may overlap. |
| S3 | Uploads in progress, input/output objects, and deletion effects. |
| External producer | Upstream positions, in-flight requests, observed acknowledgments, and durable checkpoints. |
| Communication | Requests sent/executed, responses observed, delayed operations, and connectivity per link. |

Separate durable effects from caller observations. A timeout can follow a successful transaction or upload. Internal steps remain independently interruptible even when one pipeline task performs them. For each transition, identify preconditions, volatile/durable effects, synchronization, retry behavior, and recovery responsibility.

## Action inventory

These are review actions, not one serial schedule. Split independently interruptible effects during detailed review. Expand every remote call into request delivery, execution/commit, response delivery, and caller observation. Include multipart parts/completion where used.

| ID | Action |
|---|---|
| IN-01 | Receive a request or retry, select an ingesthree destination, and forward documents with the required header. Reject a missing or incorrect header before admission or writes. Include stale routing views, partial delivery, and cancellation. |
| IN-02 | Resolve index and validation configuration. |
| IN-03 | Validate documents and classify per-document failures. |
| IN-04 | Reserve write-queue space and local-storage capacity or reject intake. Preserve the reserve for finishing, recovery, and drain. |
| IN-05 | Select the active file and serialize appends against finishing. |
| IN-06 | Allocate a file ID and create local storage. |
| IN-07 | Write identity and format metadata. |
| IN-08 | Persist creation and directory metadata as required. |
| IN-09 | Append a batch, including short/interrupted writes. |
| IN-10 | Apply the persistence policy, separating flush-to-kernel from data/metadata sync. |
| IN-11 | Send acknowledgment and validation results through the router only after successful append and flush to the kernel. |
| IN-12 | Router/client observes the result or retries a rejected/ambiguous request. Include lost persist responses before the HTTP response. |
| ROT-01 | Trigger file finishing on size/timeout/decommission. |
| ROT-02 | Stop assigning writes to the old file and resolve pending appends. |
| ROT-03 | Finish contents and any required footer/integrity metadata. |
| ROT-04 | Apply the selected persistence policy to finished contents. |
| ROT-05 | Persist metadata/rename and directory changes from finishing the file, if used. |
| ROT-06 | Switch incoming writes to a fresh file. |
| ROT-07 | Schedule upload, skipping empty files. |
| UP-01 | Open and read a finished local file. |
| UP-02 | Start/retry its upload under the same identity. |
| UP-03 | Transfer the body or multipart parts. |
| UP-04 | Complete the S3 upload. |
| UP-05 | Reconcile an ambiguous upload against expected contents. |
| UP-06 | Register the file, including repeated/concurrent registration. |
| UP-07 | Observe/reconcile registration. Discard an already-indexed file. |
| UP-08 | Establish safe handoff, delete locally, persist deletion, and release capacity. |
| ASSIGN-01 | List ready or over-threshold unfinished files, oldest first. |
| ASSIGN-02 | Pipeline atomically checks that the file is still eligible, assigns it to itself, and records its processing start. |
| ASSIGN-03 | Observe/reconcile an ambiguous assignment response, including rediscovery after the retry threshold. |
| ASSIGN-04 | Start processing, allowing an earlier attempt to continue concurrently. |
| IDX-01 | Download/stream input, including interrupted reads. |
| IDX-02 | Verify identity, format, completeness, and integrity. |
| IDX-03 | Resolve the applicable mapping/configuration. |
| IDX-04 | Parse/map/transform documents and account for invalid ones. |
| IDX-05 | Index each batch in RAM and account for builder memory. |
| IDX-06 | Establish that the entire input was processed. |
| IDX-07 | Finalize splits to disk and package outputs, including zero-output completion. Account for scratch space and release RAM safely. |
| OUT-01 | Choose output identities associated with the file/attempt. |
| OUT-02 | Stage output metadata if retaining the current API. |
| OUT-03 | Upload each output object. |
| OUT-04 | Observe/reconcile required upload results. |
| PUB-01 | Submit completion with the full output set. |
| PUB-02 | Check every contributing file/index identity and unfinished status inside the transaction. Reject the group if any input is completed. The latest assignment grants no publication priority. |
| PUB-03 | Publish the complete shared output set and complete all contributing files in that transaction. |
| PUB-04 | Commit the winning publication or return already completed to later attempts. |
| PUB-05 | Reconcile an ambiguous commit, identifying the winning outputs before cleanup. |
| PUB-06 | Dispatch post-publication work, including merge notifications. |
| CLEAN-01 | Establish durable authority to delete input. |
| CLEAN-02 | Delete input from S3 and reconcile interrupted deletion. |
| CLEAN-03 | Remove local downloads/builders/temporary outputs. |
| CLEAN-04 | Identify abandoned uploads and unpublished outputs eligible for cleanup. |
| CLEAN-05 | Remove eligible objects/uploads/metadata safely against concurrent publication. Mechanism remains open. |
| CLEAN-06 | Examine completion-evidence retirement only if a policy is introduced. Baseline retains it. |
| REC-01 | Discover surviving local files and their identities. |
| REC-02 | Resolve interrupted writes/finishing. Surface unexpected corruption. |
| REC-03 | Finish recovered files under the selected persistence policy and start fresh files. |
| REC-04 | Reconcile with S3/metastore and resume handoff or cleanup. |
| REC-05 | Retry unfinished work from the beginning while other pipelines continue. |
| REC-06 | Restart indexer pipelines and resume ordinary eligible-file discovery. |
| LIFE-01 | Stop ingester intake and drain remaining files. |
| LIFE-02 | Stop a pipeline/indexer, finishing work or leaving it for later retry. |
| LIFE-03 | Retry after permanent worker loss, while allowing a paused original attempt to resume and publish first. |
| LIFE-04 | Change mapping/configuration while work is outstanding. |
| LIFE-05 | Delete an index or recreate its name while old work still exists. |

Review labels previously called `CLAIM-*` and `GC-*` are now `ASSIGN-*` and `CLEAN-*`. Cleanup actions describe responsibilities, without selecting a collector. The retry threshold permits overlapping attempts and does not revoke publication rights.

## Failure coverage

For every applicable action, examine:

- Crash before, during, or after its effect but before observation. Repeat failures during recovery itself.
- Separate pipeline failure, process/container crash, pod replacement, and host/OS crash. Userspace buffers can be lost on process crash. Flushed kernel page-cache data can survive while the host and files remain available. Pod replacement can remove the volume even on a surviving host.
- Lost, delayed, or repeated requests/responses. A paused worker can resume after replacement.
- Independent link failures: client–router, router–ingester, ingester–S3/metastore, indexer–S3/metastore, and any chosen discovery/peer links. Include the in-process persist path when the router selects itself.
- Short writes, synchronization failures, write-queue exhaustion, local-storage exhaustion, and slow uploads/registration/publication. Keep S3 service-outage handling outside the present scope.

Interleave appends with file finishing; intake with recovery; registration retries with completion/deletion; elapsed-time retries with late completion; publication with output cleanup; and mapping/index changes with outstanding work. Include lost assignment replies while the indexer stays alive and files completing out of order on independent pipelines. For sources, examine crashes between acknowledgment and checkpoint persistence. Replay duplicates are allowed, and checkpoints must preserve unacknowledged work.

Required durability cases, with host and volume surviving: append → crash before flush must produce no successful acknowledgment; append → flush → acknowledge → producer saves checkpoint → process crash must preserve the acknowledged input. The current implementation's delayed `Flush` can allow acknowledgment before flush. The proposed contract forbids that sequence. At-least-once retries cannot recover lost acknowledged data after checkpoint advancement.

Required deletion cases: split upload succeeds → crash before publication must retain the input; atomic publication/completion commits → crash before input deletion must leave enough metastore evidence to retry deletion. If the proposed periodic cleanup is selected, verify deletion retries after crashes, failed deletes, and lost delete responses without removing completion evidence.

Required overlap case: attempt B starts after attempt A crosses the retry threshold, then A publishes first. B must observe completion and discard only its own outputs. Also delay an assignment request until after completion: it must not resurrect the file.

Required group overlap case: groups `{A,B}` and `{B,C}` compete. After `{B,C}` commits, reject all of `{A,B}` and preserve the winner's shared outputs. File `A` remains unfinished and can complete through a later attempt. A lost publication response must be reconciled before deleting shared outputs. Exercise zero-output group completion as well.

Required admission cases: fill the write queue while disk has room; retain files under slow upload/registration until the local-storage budget fills while the queue is empty. Both must reject new intake without compromising accepted data or its drain reserve. Concurrent requests must respect both budgets. Trace reservation release on rejection, cancellation, write completion, and safe local deletion. Co-located finalized splits must not consume ingest-reserved storage.

Required routing cases: exercise same-AZ priority, existing-index affinity, the small self bonus, unavailable/draining destinations, and stale capacity feedback. Destination admission remains authoritative. Multiple writers under differing router views are valid. Measure the 100 MB / under-10-second file-filling target separately from safety, including low-volume and multi-AZ traffic where smaller files are expected.

Required multi-index request cases: documents enter separate per-index files, with repeated requests accumulating in the same active file where routing permits. One file may publish while another fails and retries. Preserve the successful publication and the unfinished file independently. Cross-index atomic publication is outside the contract. Request retries may duplicate previously accepted documents.

## Invariants to validate

- **S1:** acknowledged input must stay recoverable until publication through process/container crashes while the host and storage remain available. Acknowledgment requires successful flush to the kernel. Host-crash durability, pod-replacement storage, and index-deletion policy remain open.
- **S2:** each file contains data for exactly one index incarnation. Its ID retains that incarnation and its finished contents across retries.
- **S3:** completion requires processing the entire file under the applicable document-error policy.
- **S4:** the first publication transaction to commit contributes the file's only committed output set. Client/source retry duplicates across different files are separate.
- **S5:** publication and completion of all contributing files are atomic. Published shared outputs have matching storage objects.
- **S6:** competing attempts preserve each other's output objects. Losing-attempt cleanup preserves the winning outputs.
- **S7:** cleanup requires durable handoff or terminal disposition. An ambiguous response supplies neither.
- **S8:** conditional assignment, retry timing, and completion remain coherent under concurrency. Completed files remain terminal.
- **S9:** admission respects write-queue and local-storage limits, including reservations, and preserves recoverability and the capacity to finish/drain accepted data. Co-located indexing scratch space must preserve this capacity.
- **L1:** processable unfinished files can complete once dependencies, resources, and recovery are available.
- **L2:** a failed pipeline leaves unrelated pipelines able to continue.
- **L3:** durable information supports eventual cleanup despite lost responses/notifications.
- **L4:** permanent failures have actionable outcomes rather than silent endless retry.

Progress requires stated availability and scheduling assumptions. No finite delay or timeout alone proves a worker has stopped.

## Review conclusions

- **Agreed assignment recovery:** oldest-first discovery of ready or over-threshold files, with first-publication-wins completion. Validate lost responses, overlapping attempts, late assignment requests, and worker replacement against this rule.
- **Storage obligations:** validate flush-before-acknowledgment and recovery after interrupted appends through process/container crashes with host and storage available. Decide host-crash durability and pod-replacement storage separately. Validate write-queue and local-storage admission independently, including co-located split finalization. Both must preserve a path to drain existing data. See the [persistence discussion](design_answers.md#flush-fsync-and-pod-crashes).
- **Late uploads:** already-indexed registrations are discarded. An upload finishing without registration can leave an unused S3 object. Cover this in cleanup/recovery, without treating it as duplicate indexing.
- **Mapping:** use the current per-document behavior as the reference described in the decision log. Index deletion and permanently unprocessable files still need dispositions.
- **Optional reuse check:** existing GC lists staged outputs, then can mark them deleted after they become published. If reused, cleanup and publication need conditional, mutually exclusive transitions. See [candidate selection](quickwit/quickwit-index-management/src/garbage_collection.rs#L115) and [deletion marking](quickwit/quickwit-metastore/src/metastore/postgres/metastore.rs#L1072).

## Evidence and next steps

For each case record: **action IDs → assumptions → failure trace → surviving durable facts → recovery path → invariant/result**. Distinguish an argument, an unresolved rule, a counterexample, and a reproduced failure.

Review in dependency order: local durability, input handoff, assignment/recovery, publication, cleanup/lifecycle. Present one consequential decision at a time. The [TLA+ models](quickwit/quickwit-ingest/src/ingesthree/tla/README.md) separately check single-file handoff/cleanup, overlapping multi-file publication groups, and uploader-enabled startup/finish/drain. Group checks include rejection when any contributing input is completed and recovery of unfinished leftovers. Lifecycle checks require recovery upload, registration, local deletion, and file-charge release before intake. They also check drain completion and directory ownership during cancelled startup/drain deletion. These are finite safety checks, not a composed end-to-end or Rust refinement proof. Their documentation records assumptions, exploration limits, reachability/mutation checks, and verification results. Implementation-level tests must verify their filesystem, S3, and metastore assumptions.

Implementation and experiments remain pending explicit confirmation of shared understanding.
