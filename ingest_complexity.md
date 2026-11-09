# Current ingestion vs. ingesthree: complexity assessment

## State and transition counts by category

| Inventory category | Current states | Current transitions | ingesthree states | ingesthree transitions |
|---|---:|---:|---:|---:|
| HTTP intake/routing (`intake`) | 28 | 38 | 3 | 2 |
| WAL (`wal`) | 37 | 53 | — | — |
| Local files (`local_files`) | 3 | 3 | 12 | 16 |
| Live fetch streams (`stream`) | 23 | 31 | — | — |
| File handoff (`handoff`) | — | — | 4 | 4 |
| Assignment/source state (`assignment`) | 11 | 22 | 3 | 5 |
| Indexing execution (`indexing`) | 29 | 46 | 6 | 10 |
| Publication/progress delivery (`publication`) | 44 | 54 | 8 | 6 |
| Reclamation/cleanup (`reclamation`) | 35 | 66 | 6 | 6 |
| Control/lifecycle/supervision (`control`) | 53 | 83 | 5 | 6 |
| Runtime/actor mechanics (`runtime`) | 37 | 63 | 6 | 9 |
| **Total inventoried** | **300** | **459** | **53** | **64** |

These count **named local states and distinct state-changing directed edges**, not owners or subjective difficulty ratings. One owner can contain many states, transitions and recovery rules. Owner consolidation alone does not establish simplicity.

**The ingesthree inventory is incomplete and mixes implemented code with design contracts.** The original audit is pinned to `12724a544`. The planned [pipeline supervisor](design_answers.md#pipeline-supervision), pinned separately to `83faf58e1`, adds **5 states / 6 transitions**: restart permission and pipeline-handle/backoff ownership. These are design counts, not implementation counts. Intake still covers only the local append result, not complete routing/admission. Further lifecycle, executor and optional stuck-task detection details remain unenumerated. These totals do not support an equivalent-detail reduction percentage.

**— means no graph in that ledger category, not zero complexity.** Boundaries differ: current input handoff is counted under live fetch streams; proposed WAL-style recovery is under local files. Current local-file entries cover split scratch location, while proposed entries cover input-file ownership, paths and recovery. The proposed control count covers the planned pipeline supervisor, not every remaining service-lifecycle responsibility.

The table groups the existing ledger `category` fields without changing their states or edges. These categories differ from the [15 responsibility sections](#responsibility-comparison), whose mechanisms span several graphs. Reproduce both ledger and category totals with `python3 docs/internals/count_ingest_states.py`. [Counting rules and coverage](#counted-state-and-transition-inventory) follow below. A separate [model-checked state-space comparison](#model-checked-state-space-comparison) counts reachable configurations for a matched input-reclamation slice.

## Result

**The proposed design removes most ingest/indexing coordination. Publication is one conditional metastore transaction, handoff is upload → register → delete locally, and cleanup is set reconciliation. These are small protocols, not replacements of comparable complexity for the shard/WAL/actor machinery.**

| Comparison | Current ingestion | ingesthree selected design |
|---|---|---|
| Local state types in the [counted inventory](#counted-state-and-transition-inventory) | **300** source-backed types | **53** implemented/specified types; incomplete runtime implementation |
| Distinct state-changing transitions in that inventory | **459** | **64** implemented/specified transitions; implementation will add detail |
| Initial-indexing owners per pipeline / attempt | **8 stage actors + 1 supervisor** | **1 attempt task**, with worker supervision outside it |
| Stage/supervisor mailboxes and priority channels | **9 mailboxes / 18 channels** per pipeline | **0 inter-stage mailboxes** inside the attempt; admission and service queues remain |
| Adjacent stage handoffs | **7**, plus assignment, publication/truncation feedback and supervision | **0 inter-stage handoffs** inside the attempt; external IO and publication remain |
| What can be waiting at the same time | Different stages hold different batches: one can wait for downstream space while another waits for upload or publication | One task processes each file in sequence, with no queues between its processing steps |
| Managing shards and deciding how many pipelines to run | Create/close/place shards, apply `min_shards`, group shards into pipelines, then start/stop/reassign pipelines | **Removed:** no ingest shards or shard-to-pipeline scheduler |
| Recovering local input after a crash | Repair/replay the WAL | Same responsibility: recover accepted records from per-index append files. Both are WAL-style recovery |
| When local input can be deleted | After indexing publishes and truncation feedback reaches the ingester | After the file is uploaded to S3 and registration is confirmed, independently of indexing |
| State/transition evidence | [Actual owners, execution conditions, waits, effects and failure edges](#source-backed-states-and-transitions) | Implemented local-file core is distinguished from distributed design contracts below |

**Decisions reflected here:** per-index append files finished and uploaded unchanged; admitted writes continue after caller disconnect, as today, with budgets following actual work; existing mapping behavior without version pinning; page-cache bypass is optional. File format, IO mode and tuning are implementation choices, not prerequisites for this comparison.

The strongest evidence is structural: removal of shard placement, shared-WAL extraction/reclamation, streaming progress/revocation, ordered checkpoint handoffs, and the eight-stage actor interaction protocol. File handoff recovery retries from the last established step. A lost reply requires checking the file's recorded state, not introducing another coordination authority. The numbers above count the explicit local graphs below. They are inventory totals, not global configurations or an equivalent-detail implementation ratio.

Detailed analysis below: [actors and mailboxes](#actors-and-mailboxes), [individual handoffs](#inter-stage-handoffs), [WAL locks and runtime execution](#wal-locks-and-runtime-execution), [serializer OOM](#serializer-scheduling-and-oom), [truncation stalls](#truncation-blocked-behind-later-input), [resource-release requirements](#resource-release-requirements), and [historical evidence](#historical-evidence).

Related design documents:
- [Architecture comparison](ingest_comparison.md) and [decision log](design_answers.md).
- [Correctness validation](validation.md): action inventory, invariants, and separately documented TLA+ safety checks. Full implementation-level failure coverage remains pending.

### Evidence and scope

Four initial parallel reviews covered current protocols, actors/mailboxes, proposed recovery, and history. Two more traced the reported serializer OOM and blocked truncation. A further pair audited WAL locks/runtime execution and omissions: node-wide critical sections, detached request lifetimes, synchronous replay, recovery memory and observability were materially underrepresented. Those findings are now explicit. The inventory is source-backed but does not establish that every component, state combination or failure mode has been captured.

Original current-path baseline: `f6447683b92d4a4fbf4bee694fd60d651405ebf9`. The original state/transition audit reads `simplify-ingest` at `12724a5447666808ff486dc3472176388936d197`, including the experimental ingesthree local-file core. A separate design-only supervision ledger reads `83faf58e19172e3aa65afd41c6cd7df7b10bfaea`; the original source-backed counts are unchanged. Replication was already removed. Distributed ingesthree behavior is a design contract, not implemented task topology. Source and test inspection establishes mechanisms; Git establishes historical fixes. The user's serializer OOM and truncation incidents are first-hand reports. Exact deployed revisions, scheduling sequences and peak allocations remain unreconstructed. The earlier synthetic enumerator and its totals have been removed: invented scheduling bounds and omitted owners made them unsuitable for this comparison. This replacement is a code-path audit. Rust tests, production telemetry queries, crash experiments and benchmarks were not run for it.

The core comparison covers Quickwit's HTTP ingest path, initial indexing orchestration, and input/output recovery. Intrinsic index construction, retained merge execution, and connector-specific producer protocols are reported separately. Shared S3/metastore internals are treated as service contracts on both sides. Their interactions with Quickwit are included.

## Method

### What to count and compare

Assess **the owners, states and dependencies a maintainer must understand to keep each responsibility correct**. Keep the same 15 responsibility IDs for traceability. Record four kinds of evidence without adding unlike units:

| Evidence | Counting / comparison rule |
|---|---|
| Components and owners | Name separately scheduled actors/tasks and service roles. Distinguish types from instances and state the scope: per pipeline, per shard, per node or shared. A role implemented inside another service is not another process. |
| State | Record lifecycle, execution/wait condition, retained resources, authoritative state and cached views. Include implicit states such as backpressure and ambiguous outcomes, not just enum variants. |
| Coordination and correctness | Identify who changes each fact, who must observe it, ordering requirements, and feedback/control loops. Several interacting rules must remain visible rather than saturating a numerical category. |
| Recovery | Record reachable failure/interleaving classes that require different recovery rules and the durable evidence authorizing retries, publication or deletion. |

### Counted state and transition inventory

**Use the transition count as the headline coordination metric: 459 documented state-changing transitions in current ingestion.** The ingesthree core and specified replacement protocols, including pipeline supervision, currently enumerate **64**. Their corresponding local-state counts are **300** and **53**. Every counted state and transition is named in the linked ledgers. This makes omissions and added implementation machinery visible instead of assigning subjective weights.

| Ledger portion | Current states / transitions | ingesthree states / transitions |
|---|---:|---:|
| Shared runtime execution vocabulary, counted once per side | 6 / 9 | 6 / 9 |
| Actor framework, counted once rather than once per stage | 33 / 55 | 0 / 0 inside the file attempt |
| Initial-indexing actors, uploads and supervisor | 83 / 122 | Included in the specified file-attempt protocol below |
| HTTP intake, routing, WAL, fetch, feedback, reclamation and shard/plan control | 178 / 273 | No equivalent shard/live-WAL protocols; retained intake detail is not yet fully enumerated |
| Implemented per-index local-file core | Included in current WAL inventory at different implementation detail | 15 / 18 |
| Specified file handoff, assignment, attempt, publication and cleanup | Included across the current owners above | 27 / 31 |
| Specified pipeline supervision, added from `83faf58e1` | Included in initial-indexing actors/supervisor above | 5 / 6 |
| **Total explicitly inventoried** | **300 / 459** | **53 / 64** |

**Exact counting rule:** a state is a named value of one local control, wait, ownership, lifecycle or durable-fact dimension. Its identity is `(graph, state)`. Shared ready/running and reused completion nodes are aliases, counted once. A transition is a distinct directed `(graph, from, to)` edge with different endpoints. Alternative triggers for the same edge count once. Retries/refusals that leave that dimension unchanged remain documented but add no state-changing edge. One real event can update several dimensions, so this counts local transition rules, not globally distinct events. Common actor mechanics are counted once, while each owner's distinct dependency is retained. Separate CPU functions under the same owner are not automatically states. No batch count, CPU-slot bound, queue-occupancy product or BFS is involved. State and transition totals remain separate rather than being added into a score.

Reproduce: `python3 docs/internals/count_ingest_states.py`. The script validates names, endpoints, shared-node references and duplicates, then counts the handwritten [intake/WAL ledger](docs/internals/ingest_states/intake.json), [indexing/runtime ledger](docs/internals/ingest_states/indexing.json) and [ingesthree ledger](docs/internals/ingest_states/ingesthree.json), plus the later [pipeline-supervision ledger](docs/internals/ingest_states/pipeline_supervision.json). Each graph contains its states, transition triggers, sources and qualifications. It does not simulate execution.

**Coverage:** current entries come from committed `12724a544`. The proposal combines that snapshot's 15 implemented local-core states, 27 specified/proposed states and six shared runtime states with five supervision states from the design at `83faf58e1`. The original proposed subtotal was 48 states / 58 transitions; supervision adds 5 / 6. Later async-wrapper implementation is outside the original snapshot. Current physical WAL-file GC/replay detail, exact cache/queue occupancy, and administrative/connector paths remain outside the numeric inventory. Proposed routing/admission, executor/offload details, active-pipeline drain/join behavior, optional progress checks, notification recovery and index-deletion handling remain partly unimplemented/unenumerated. Shared DB/S3/OS internals and intrinsic indexing/merge algorithms are outside both. These are auditable inventory totals, **not a claim of complete coverage or “87% less complexity.”** The target number will grow as retained implementation details are included. The simpler publication/handoff/cleanup protocols below already have their small explicit counts; they are not assigned speculative complexity penalties.

#### How small the replacement protocols are

| Protocol dimension | States | State-changing transitions | Rule |
|---|---:|---:|---|
| Input handoff | 4 | 4 | Finished local → upload established → registration established → local deleted. Recovery can establish registration directly. Retry failures retain the current established step. |
| Durable file completion | 2 | 1 | Unfinished → completed, atomically with all output publication. The metastore chooses the winner. Losing/repeated calls leave completion unchanged. |
| Publication caller's result | 3 | 2 | Unresolved → own success or another winner established. This protects the winning outputs after a lost reply. |
| Completed-input cleanup | 3 | 3 | Retained → deletable → absent; a late redundant upload becomes deletable again. |
| Losing-output cleanup | 3 | 3 | Retained → deletable → absent; a late safe-to-delete object becomes deletable again. |
| Supervisor restart permission | 2 | 1 | Enabled → disabled when drain or shutdown begins. Disabling restarts does not imply active tasks have stopped. |
| Pipeline-slot ownership | 3 | 5 | No task → owned handle → restart backoff → replacement handle. Observed exit during drain or suppressed pending replacement returns the slot to no task. |

These are subsets of the totals, not additional scores. Cleanup is small set arithmetic: completed-input candidates are `present inputs ∩ completed file IDs`. Once a file is completed, losing outputs are `that file's attempt outputs − winner outputs`. For unfinished work, preserve outputs that a still-allowed attempt can publish. Similarly, absence of registration alone does not authorize deleting an upload whose registration can still arrive. Those are predicates defining the protected set, not evidence that cleanup needs a complex subsystem. Repeating the same reconciliation handles failed deletes and late objects.

### Model-checked state-space comparison

The local-state inventory above counts named dimensions and their transition rules.
The paired [input-reclamation model](docs/internals/ingest_states/tla/README.md)
counts **reachable combinations** for a smaller, matched protocol slice. These are
separate measurements. Their totals are neither added together nor converted into
an overall complexity-reduction percentage.

**Configuration:** one index, one or two ordered work units, already flushed and
acknowledged. V2 uses publication intervals in one shard. Ingesthree uses finished
singleton files. Both use the same abstract output-production step and ordered
publication send/commit/observation. Remote operations and responses may be delayed.
Crashes, retries, assignment, worker/actor topology, queue capacities, and physical
WAL segment GC are excluded. There are no state/action constraints, symmetry
reductions, or verification-only history variables. Unused protocol fields remain
constant. File publication/remote cleanup are selected design contracts.

| Work units | V2 reachable states | ingesthree reachable states | V2 / ingesthree search depth |
|---:|---:|---:|---:|
| 1 | 13 | 29 | 11 / 14 |
| 2 | 86 | 163 | 20 / 27 |

Both configurations passed the recoverability, output-existence, publication,
feedback-authority, and reclamation checks with TLC fingerprint polynomials 0 and
1. Search depth is diagnostic. Counts depend on the stated action boundaries and
publication-order restriction, which is an execution subset of ingesthree.

**Ingesthree has more reachable states under this encoding.** Upload/registration
and their responses add intermediate states. Local and remote deletion can proceed
at different points. This introduces a storage handoff while removing the dependency
of local reclamation on indexing/publication. The totals include all modeled
mechanisms; they do not isolate the numerical contribution of any one mechanism.
The v2 stage actors, shard planning, and supervision behind much of the broader
simplification are outside this slice.

The model also produces concrete witnesses:

- **V2:** publication has succeeded and its truncation suggestion is queued while
  Source is still handling later input. Both direct RPC and gossip depend on Source
  processing that suggestion. This shows a feedback dependency, not a deadlock or
  proof that two literal batches fill the real ten-slot downstream mailbox.
- **Ingesthree:** local input is deleted after confirmed registration while its
  publication is still pending. File indexing can proceed before the uploader sees
  registration's response. Remote cleanup can proceed before the publisher sees
  publication's response.
- **Both:** a state with all inputs reclaimed after publication is reachable.
  Eventual progress is outside this safety model.

Reproduce from the repository root with `TLA_JAR` pointing to the official tools:

```bash
python3 docs/internals/check_ingest_reclamation.py --probes --mutations
python3 docs/internals/check_ingest_reclamation.py --fingerprint 1 --probes --mutations
```

The runner retains configurations, logs, and witness traces outside the repository.
The [model documentation](docs/internals/ingest_states/tla/README.md) maps actions
to implementation/design evidence and records exclusions. Mutation tests verify
that incorrect deletion authority, bypassed Source feedback, and publication
without output are detected. The original **300/459 versus 53/64** local inventory
remains unchanged. Broader ingesthree model counts have different scopes and are
not comparable to this table.

### Source-backed states and transitions

**Read each owner together with its execution state, held resources and external effects.** “Serializer awaiting scheduling with live writers” and “Serializer blocked sending finalized output” differ even though both have lifecycle `Running`. One owner can execute several functions without changing its ownership or wait condition. Conversely a single function can wait on several different dependencies. Arrows below describe real transitions, not an assertion that every call suspends. `W(x)` means the future can return `Pending` on x; an immediately ready future continues in the same poll. Scope: HTTP ingest-v2, its WAL/fetch helpers, initial indexing and associated control/recovery. Retained merges and alternative connectors are separate below. SDK, kernel and service internals are represented at their actual call boundaries, not invented internal states.

#### Execution and dispatch, shared by the owners below

| Condition | Entry / exit transition | What remains owned |
|---|---|---|
| Runnable, waiting for executor scheduling | Spawn, wake, or cooperative yield makes work eligible → executor starts/resumes polling | Task captures, received envelope, partial handler state, held guards and permits |
| Executing on CPU | Executor polls task/starts closure → continues synchronously, finishes, yields, or polls a dependency that returns Pending | Whatever that operation has acquired; `ActorState::Running` alone does not imply this condition |
| Suspended on a named async dependency | Lock/channel/permit/watch/RPC/timer/job result returns Pending → dependency wake makes task runnable → a later poll observes readiness or waits again | Resources across the await remain held; the runtime worker is available for other tasks |
| Thread blocked inside synchronous work | File IO, synchronous mutex or blocking IO limiter blocks → syscall/lock/limiter returns → same call continues | Worker thread and task resources stay occupied; this is not a Tokio suspension |
| Runnable thread, OS-descheduled | OS preempts the executing worker → OS schedules it again | Poll/closure has not yielded and still owns its resources; distinct from a Tokio task awaiting dispatch |
| Offloaded job queued / executing / result returned | Rayon or blocking submission → worker runs closure → result waits for async caller → caller resumes and consumes/restores it | Captures survive queue delay; completed results can retain resources while caller awaits scheduling |
| Completed / cancelled / panicked | Return, actual abort/drop, or unwind → release local owners according to drop/finalize rules | Detached work and durable effects can survive; cancellation is not rollback |

[Runtime selection](quickwit/quickwit-common/src/runtimes.rs#L52), [actor spawning/dispatch](quickwit/quickwit-actors/src/spawn_builder.rs#L167), [Rayon submission](quickwit/quickwit-common/src/thread_pool/simple.rs#L73), [WAL offload/restore](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L75), [synchronous IO controls](quickwit/quickwit-common/src/io.rs#L311). Main Tokio, actor Blocking/NonBlocking runtimes, Rayon and Tokio blocking workers are different executors. There is no shared two-slot CPU scheduler in this code.

**Actor transitions:** spawn → initialize → receive envelope → handler → drain available envelopes → drained hook → receive again. Pause/Resume changes dispatch eligibility. Outer Paused receive accepts high priority only; a Pause encountered inside an already-running drain loop does not immediately stop that loop's low-priority draining. Success/ordered exit, Quit, downstream closure, observed kill or returned error leave dispatch and run the applicable finalizer; panic unwinds and join reports Panicked instead of guaranteeing that normal finalizer path. Last external mailbox gone plus empty inbox enables success. Source, Processor and Indexer disable per-message yields; Serializer and other default actors yield before handlers, after removing the envelope. Every actor yields during the drained-mailbox probe. CPU loops are not automatically preempted by commands. [Dispatch](quickwit/quickwit-actors/src/spawn_builder.rs#L279), [commands](quickwit/quickwit-actors/src/command.rs#L25), [join](quickwit/quickwit-actors/src/registry.rs#L225).

**Channel transitions:** sender owns payload → admission waits if full/no rendezvous receiver → queue or receiver owns envelope → handler owns payload → next send or drop. Empty input wait is the actor's idle condition, not another independent flag. A blocked sender, current handler, queued messages and displaced low-priority envelope can all own work simultaneously. High-priority messages wait for the current handler to finish. Receiver drop destroys queued work and fails pending sends; reply-sender drop resolves reply waits with error. Scheduled timers retain mailbox references and enqueue high-priority messages when due. Health `NoUpdate/Updated/ProtectedZone` is independent: protected waits can remain healthy without forward progress. [Channels](quickwit/quickwit-actors/src/channel_with_priority.rs#L127), [mailbox](quickwit/quickwit-actors/src/mailbox.rs#L173), [timers](quickwit/quickwit-actors/src/actor_context.rs#L340), [health](quickwit/quickwit-common/src/progress.rs#L25).

#### Intake, WAL ownership and persistence

| Owner / retained state | Real transitions, waits and failure edges | Code |
|---|---|---|
| Router request and byte permit | Try admission → immediate rejection or retain bytes/permit → routing attempts → Auto response/timeout, or WaitFor/Force publication wait → release. Caller timeout can precede completion of detached destination work. | [Admission](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L573) |
| Routing attempt, batches and debounce guards | W(router mutex) → inspect node eligibility → discovery winner calls CP; competitors W(read barrier) → W(router mutex) → choose nodes → concurrent per-node persist futures → partial outcomes/update routing → retry unresolved requests, up to five attempts. Per-node timeout is ambiguous; index/source-not-found is terminal. | [Router](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L157), [workbench](quickwit/quickwit-ingest/src/ingest_v2/workbench.rs#L175) |
| CP shard creation and indexing plan | Choose placement → concurrent init RPCs create local queues → metastore open-shards → CP model update. Partial/uncertain opens need reconciliation. Shard/load changes → recompute placement/grouping → desired plan → apply start/stop/reassign → report applied tasks → reconcile drift. | [Open](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L548), [plan](quickwit/quickwit-control-plane/src/indexing_scheduler/mod.rs#L391), [apply](quickwit/quickwit-indexing/src/actors/indexing_service.rs#L658) |
| Publication tracker and callbacks | Persist success registers shard target → AwaitingPublish → position event reaches target → Published/Notify → waiter resumes. Callback/task ordering is independent. Unknown-shard events and repeated targets are currently ignored, creating the static hazards described below. | [Tracker](quickwit/quickwit-ingest/src/ingest_v2/publish_tracker.rs#L33) |
| Service admission / detached mutation task | Circuit breaker Closed → failures open it → cooldown → HalfOpen → result changes breaker state. Accepted call spawns inner task; caller waits JoinHandle. Caller departure detaches rather than aborts mutation. Open-fetch has different middleware. | [Wiring](quickwit/quickwit-serve/src/lib.rs#L1075), [breaker](quickwit/quickwit-common/src/tower/circuit_breaker.rs#L99), [detachment](quickwit/quickwit-common/src/tower/one_task_per_call_layer.rs#L72) |
| Ingester initialization | Initializing → W(WAL write), then W(state mutex) → spawn synchronous WAL open/replay on Tokio worker → W(join), retaining both guards → recover closed shards → Ready. Failure → Failed. Normal requests reject Initializing; background exact-Ready waiters can remain pending after failure. | [Startup](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L232), [open](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L56) |
| Persist task before mutation | W(WAL write) → hold WAL write while W(state mutex) → check Ready/Retiring, select open shard, check WAL budgets, try rate tokens. Refusals return immediately; they are not capacity/rate wait queues. Request bytes survive both lock waits. | [Persist](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L412), [lock order](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L362) |
| Validation job plus persist continuation | Both guards held → submit batch/mapper to Rayon → queued / running → result ready / caller awaiting scheduling → resume. Invalid docs yield per-doc failures; no valid docs means no append; job error propagates. A running closure survives caller cancellation. | [Validation](quickwit/quickwit-ingest/src/ingest_v2/doc_mapper.rs#L73), [submission](quickwit/quickwit-common/src/thread_pool/simple.rs#L73) |
| WAL mutation job plus persist continuation | Take log from wrapper (`Some → None`) → submit blocking job → queued / executing serialization and filesystem IO → result/log returned → caller resumes, restores `Some`, advances position/watch → releases full guards. Append subrequests run sequentially; some can succeed while others fail. IO error closes affected shard; missing queue removes dangling metadata. | [Ownership](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L75), [append/results](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L570) |
| Physical writer, memory queue and durability | Serialize/write frames → policy flush if due → insert copied records into memory queue → append returns. Accepted in-memory data can still be userspace-buffered on disk. Five-second delayed Flush is checked on append/truncate, not a background flush timer. Create/delete/rollover can fsync. Partial IO errors can leave disk/memory effects that recovery must reconcile. Abandoned restoring continuation can poison wrapper; later access aborts. | [Pinned append](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L174), [policy](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/persist_policy.rs#L62), [poison](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L35) |
| Init-shards / mapper creation | W(full guards) → synchronous mapper construction → W(create-queue blocking job) → insert open shard. Existing local shard succeeds idempotently; WAL queue without metadata errors. Guards remain held across queue IO. | [Initialization](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L758) |

#### In-memory WAL streaming to the indexer

**Direction: ingester → indexer.** The indexer's Source requests a stream. An ingester-side fetch task copies records from the WAL's in-memory queue and sends them back. There is no ingester-to-ingester replication here. Fetching is an independent owner that can coexist with persistence, subject to the WAL lock.

```text
indexer Source --OpenFetchStream--> ingester fetch task
WAL memory queue --copy under read lock--> fetch-owned batch
  --release read lock--> server response queue (3 messages per stream)
  --local service stream or gRPC--> indexer per-shard forwarding task
  --> shared MultiFetch queue (3 messages total per Source)
  --> Source-owned accumulated batch --> DocProcessor mailbox (10 messages)
```

| Owner / state | Trigger → next state; resource consequence | Code |
|---|---|---|
| Open-fetch request | W(metadata mutex) → find shard/mark advertisable → create channel/spawn fetch task → return stream; missing shard errors. Metadata guard is released before streaming. | [Open](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L716) |
| Server fetch waiting for records | Drained open queue → W(shard watch). Append/close notification → runnable → W(WAL read lock). Backlog skips watch wait. Deleted shard/watch → error delivery/end. Client disconnect alone does not wake the watch wait. | [Watch/read](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L104) |
| Server fetch reading WAL | Read lock acquired → synchronous CPU copy into separate batch → release read lock → construct payload/advance cursor → W(response send). No disk read/offload occurs in this copy loop. Writers exclude copying; multiple readers can coexist. | [Copy/unlock](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L129) |
| Server fetch sending / backpressured | Response queue has room → enqueue → next batch/watch. Full queue → retain completed batch while waiting, **without WAL read lock**. Consumer drop → return. Closed shard fully drained → W(EOF send) → finish. Queue disappearance → W(error send) → finish. | [Send/EOF](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L180) |
| Per-shard indexer fetch task | W(open RPC) → W(stream item) → W(shared MultiFetch send) → advance forwarded cursor only after admission → next item. Error is sent through same bounded channel, then retry sleep/reopen. ShardNotFound/EOF ends retries. Source reset/drop aborts these tasks and clears buffered responses; server watch wait may survive until its next event. | [Retry/forward](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L380), [reset](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L344) |
| Source accumulating / forwarding | W(MultiFetch receive or deadline) → decode docs/commit/EOF, extend checkpoint → repeat to byte target/deadline → W(DocProcessor admission) → enqueue next Loop. Batch survives downstream backpressure; receive deadline does not time out that send. | [Emission](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L446) |

Each stream also has one constructing/pending-send batch beyond its three queued messages; each forwarding task can hold one pending message beyond the shared three-slot queue. Remote transport adds encoding/socket/HTTP2 buffers and waits, while a colocated call bypasses gRPC. The 1 MiB fetch target and 5 MiB Source target allow whole-record/payload overshoot. Source read/forwarded positions are separate from published/truncated positions. [Capacities](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L74), [merge channel](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L257), [local transport](quickwit/quickwit-serve/src/lib.rs#L1283).

#### Initial indexing, publication and supervision

Every actor below also has the common receive/scheduling/dispatch/lifecycle states above. A handler waiting on output cannot service its own input/control mailbox. The workbench, publish lock, token, timers and detached tasks are independent facts, not extra mutually exclusive actor modes.

| Owner / retained state | Actual transitions and wait/failure branches | Code |
|---|---|---|
| Source assignment | Unchanged set → no-op; removal of incomplete shard/dead lock → clear assignments/abort fetches → W(old publish-lock kill) → new lock → W(forward lock). W(metastore acquire) → refresh token → subscribe from **published** position → emit known truncation. Error fails actor. Removing only Complete shards avoids full reset. | [Reset/acquire](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L379) |
| Assigned-shard status | Acquisition → Active; payload → Active/advance; fetch error → Error; later payload can recover Active; EOF → ReachedEof/checkpoint; missing shard → NotFound/EOF; published EOF suggestion or acquired EOF → Complete. EOF received does not establish publication. | [Status/handlers](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L107) |
| DocProcessor | Dead batch lock → discard; live lock → synchronous parse/transform/map loop → W(Indexer send). Bad docs are skipped with checkpoint retained. New lock replaces local lock and forwards. No per-document yield. Success/Quit finalizes with ordered downstream success; abnormal exit skips flush. | [Processing](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L406), [handlers/finalize](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L556) |
| Indexer / optional workbench | Cooperative initialization/drain can Pause until scheduled Resume. No workbench → W(optional cooperative permit) → W(delete-opstamp RPC) → create workbench/schedule commit timer. Batch → checkpoint check → synchronous indexing; dead lock drops workbench. Size/docs/Force/matching timer/cooperative drain/final flush → take workbench → W(Serializer rendezvous), carrying builders or EmptySplit. Stale timer ignored. Permit can drop before writers leave sender. | [Workbench](quickwit/quickwit-indexing/src/actors/indexer.rs#L192), [drain](quickwit/quickwit-indexing/src/actors/indexer.rs#L415), [commit](quickwit/quickwit-indexing/src/actors/indexer.rs#L593) |
| IndexSerializer | Rendezvous receives builders → framework yield/await scheduling with writers live → synchronous finalization CPU/file IO → writers/RAM release, disk outputs remain → W(Packager send). EmptySplit forwards. Errors fail actor; no per-builder async yield or publish-lock early discard. Accounting guards can outlast writer release through the blocked send. | [Serializer](quickwit/quickwit-indexing/src/actors/index_serializer.rs#L66), [finalize](quickwit/quickwit-indexing/src/models/indexed_split.rs#L151) |
| Packager | Receive disk outputs → dispatch → dead lock discards → synchronous metadata/tags/hotcache/file work → W(Uploader rendezvous). Its async processing helper has no internal await. Tag-extraction errors log/omit tags; other IO errors fail, invalid invariants can panic. | [Packaging](quickwit/quickwit-indexing/src/actors/packager.rs#L81) |
| Uploader actor | Receive packaged batch → W(Sequencer reservation admission) → W(global upload permit) → kill check → spawn independent upload task → return to input. EmptySplit reserves/completes directly, without upload task/permit. Finalizer does not join detached uploads. | [Reserve/spawn](quickwit/quickwit-indexing/src/actors/uploader.rs#L263) |
| Each detached upload task | Runnable → prepare payload, dead lock sends Discard → W(stage RPC) → W(each remote PUT) → W(cache mutex and cache scan/evict/rename, or scratch removal) → send Proceed/result → drop permit. Preparation/stage error drops sender; upload/cache error also kills generation. Already started effects can survive kill; later results can wait in oneshots after permits return. | [Task](quickwit/quickwit-indexing/src/actors/uploader.rs#L302), [store](quickwit/quickwit-indexing/src/split_store/indexing_split_store.rs#L111), [cache](quickwit/quickwit-indexing/src/split_store/indexing_split_cache.rs#L549) |
| Sequencer | Receive oldest reservation → W(oneshot result); later results cannot pass it → Proceed: W(Publisher admission); Discard: next reservation; sender dropped: failure. Both waits are protected and have no timeout/kill select. | [Sequencing](quickwit/quickwit-indexing/src/actors/sequencer.rs#L78) |
| Publisher / mutex guard / shared token | W(publish-lock mutex) → dead lock discards; live guard → W(publication RPC). Invalid token → sleep 1 s then 3 s/reload token/retry while retaining guard. Success drops guard → enqueue Source truncate → W(merge notification). Other errors fail. Exhausted token retries kill lock and request ordered Source success/drain. Lost response can leave durable publication committed. | [Retries](quickwit/quickwit-indexing/src/actors/publisher.rs#L133), [handler](quickwit/quickwit-indexing/src/actors/publisher.rs#L223) |
| Pipeline supervisor / generation | No handles → W(global spawn semaphore) → spawn stages → construct ingest-v2 Source synchronously/forward initial assignment to its unbounded mailbox → install handles. Partial startup failure schedules retry. Generic source-load/NotFound paths also exist for other source configurations. Health tick: healthy → next tick; all successful → success; failed/unhealthy → kill generation → W(selected joins) → schedule replacement. Explicit joins cover Source/Indexer/Packager/Uploader/Publisher, not all tasks. Startup retries back off to 600 s; failed running generation retries after 1 s. | [Spawn](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L323), [supervise/join/retry](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L477) |

**Normal drain, Quit and failure differ.** Ordered Source success lets Processor forward success and Indexer flush; last-mailbox drain propagates through later actors. Upload reservations can keep Sequencer waiting after Uploader exits. High-priority Quit can bypass queued ordinary work once the current handler ends. Kill is cooperative; it does not interrupt running CPU, pending IO or detached uploads. Normal returned failures activate the shared switch; a panic unwinds instead of guaranteeing that path. Framework `Running/Paused/Success/Failure`, exit reason, task execution condition, health protection and shared kill state remain separate. [Finalizers](quickwit/quickwit-indexing/src/actors/indexer.rs#L454), [exit/kill](quickwit/quickwit-actors/src/actor_context.rs#L363), [join](quickwit/quickwit-actors/src/registry.rs#L237).

#### Publication feedback, reclamation and service maintenance

| Owner / independently advancing fact | Transitions, waits and failure edges | Code |
|---|---|---|
| Source truncate handler and detached RPCs | Queued suggestion → Source dispatch after current handler → mark published EOF Complete → emit local-position event → spawn per-ingester truncate RPC tasks. Each task waits RPC/retry sleep, up to five attempts; final failure logs. Source does not await those RPCs. | [Truncation launch](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L295) |
| Event delivery and shard-positions actor | Event spawns subscriber task → W(subscriber mutex) → enqueue actor update → monotonic local position update → W(Chitchat publication) → normalized event. Remote gossip takes a parallel input path. Tasks/mailbox can accumulate; repeated positions can be deduplicated. CP model and ingester positions advance separately. | [Events](quickwit/quickwit-common/src/pubsub.rs#L175), [positions](quickwit/quickwit-indexing/src/models/shard_positions.rs#L128) |
| Ingester direct truncation / gossip cleanup | RPC W(full guards) → no-op old/missing shard, or W(truncate job). Gossip first W(partial state)/filters, yields, then W(full guards); EOF deletes, offset truncates. Underlying IO errors can log without failing RPC. **Direct EOF truncation retains shard identity; gossip EOF deletes it.** | [RPC](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L804), [gossip](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L1039) |
| WAL logical removal and physical GC | Write truncate/delete record → remove memory records/queue and file references → synchronous eligible-file GC → policy flush/fsync → restore log/advance metadata. Partial effects survive errors. Mixed-shard physical files remain until all references allow deletion; last file remains. No independent GC task supplies progress. | [Mutation/GC](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L254), [metadata](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L558) |
| Reset / retain reconciliation | Reset trigger → try single permit or skip → W(Ready) → W(full guards)/snapshot → release → W(CP advice with timeout) → W(full guards)/apply delete/truncate → release → cooldown holding reset permit. Advice reads CP model, which can lag publication. Retain RPC instead deletes queues absent from keep-set under full guards. | [Reset](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L262), [retain](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L942) |
| Local shard and ingester lifecycle | Queue creation → Open; explicit/idle close → Closed, notify fetch to drain; startup recovers Closed queues; delete removes shard. Ingester Initializing → Ready or Failed. Normal drain: Ready → Retiring (writes accepted, routing withdraws) → 15 s timer → Decommissioning (close all shards) → Decommissioned once remaining shards are empty unadvertised orphans. Fetch/truncate continue during drain. Protocol Unavailable is not a separate normal local append phase. | [Shard](quickwit/quickwit-ingest/src/ingest_v2/models.rs#L164), [idle](quickwit/quickwit-ingest/src/ingest_v2/idle.rs#L22), [drain](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L989) |
| Capacity, shard broadcast and diagnostics | Timer → W(full guards for capacity / partial state for shard rates) → snapshot → release guards → W(cluster update). Debug takes full guards. Shutdown W(decommission watch under timeout); timeout diagnostic still W(WAL read), so timeout does not guarantee immediate exit. | [Capacity](quickwit/quickwit-ingest/src/ingest_v2/broadcast/capacity_score.rs#L48), [shards](quickwit/quickwit-ingest/src/ingest_v2/broadcast/local_shards.rs#L250), [shutdown](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L117) |
| Metastore splits / object cleanup | Stage metadata → upload object → publish split/checkpoint; metadata Staged/Published/MarkedForDeletion is independent of object existence. GC discovers candidates → marks metadata → deletes object → deletes metadata, with retry after failure. CP EOF handling separately updates position, deletes shard metadata and replans. | [Split states](quickwit/quickwit-metastore/src/split_metadata.rs#L372), [GC](quickwit/quickwit-index-management/src/garbage_collection.rs#L115), [CP](quickwit/quickwit-control-plane/src/control_plane.rs#L419) |

**Real simultaneous conditions and exclusions:** persist can hold both guards while its validation job awaits CPU; fetch waits for read access and already-published input waits for truncation write access. A completed WAL job can still leave those guards held until its async caller is scheduled. Once fetch releases its read lock, it can remain blocked sending a copied batch while new appends proceed. Publisher can commit while Source is blocked forwarding later input, leaving both direct truncation and gossip unstarted. Serializer can hold received writers after its real framework yield while another Indexer runs. These combinations follow from separate owners and actual gates. A fetch send does **not** hold the WAL read lock; one ingester cannot run two full critical sections concurrently; Source cannot dispatch truncation during its blocked send; receiving EOF does **not** mean publication completed.

#### ingesthree: implemented states versus design contracts

At the audited commit `12724a544`, the implemented core is a synchronous storage object, not yet connected to production intake. It has **no internal async queue, task spawn, timer, upload or assignment worker**. Concurrent, uncommitted `AsyncIngester` work appeared during this audit; its evolving wrapper is outside this snapshot. It runs on its caller's thread, which can execute CPU, be OS-descheduled or block in filesystem calls. Offload dispatch/result waits will depend on its future integration. [Module](quickwit/quickwit-ingest/src/ingesthree/mod.rs#L15), [implementation status](implementation_status.md).

| Implemented owner/fact | Actual transitions and failure edges | Code |
|---|---|---|
| Directory owner / ability to write | Open → try exclusive file lock, reject contention → scan/recover → usable owner. Validation/budget rejection leaves it usable. Any error after mutation starts leaves directory-wide `failed=true`; later mutations reject until drop/reopen. `&mut self` serializes calls, not a hidden async mutex. | [Open](quickwit/quickwit-ingest/src/ingesthree/mod.rs#L75), [append](quickwit/quickwit-ingest/src/ingesthree/mod.rs#L154) |
| Per-index file / acknowledgment | No active file → `.creating`, write/flush identity header → rename `.open` → append checksummed records → flush → return success. Open file is reused; success means kernel flush, not fsync. File extension and whether each request received its reply are independent. | [Append](quickwit/quickwit-ingest/src/ingesthree/mod.rs#L190) |
| Finishing files | Manually finish a file → flush/rename `.finished`, release active writer → retain finished-file metadata and disk bytes; next append uses fresh ID. No active file → no-op. `finish(self)` finishes files sequentially and releases owner/lock; it does not upload/delete or complete distributed drain. | [Finish](quickwit/quickwit-ingest/src/ingesthree/mod.rs#L239) |
| Recovery | `.creating` → remove; `.open` → validate, truncate only incomplete last document frame, remove if no complete docs or finish under same ID; `.finished` → validate unchanged. Keep complete unacknowledged records. Bad checksums/headers/identity, invalid paths, finished-file truncation or IO error → fail recovery. Earlier repairs can survive a later error. | [Recovery](quickwit/quickwit-ingest/src/ingesthree/mod.rs#L89), [scan](quickwit/quickwit-ingest/src/ingesthree/documents_staged_for_upload.rs#L72) |

The following are **selected distributed contracts, not code-established task states**. Their concrete async waits, executors, concurrency controls and retry loops remain unimplemented. One attempt owner does not imply uninterrupted CPU execution, a chosen semaphore, one serial handoff owner or a prescribed staging RPC.

| Design fact / owner | Required transition or recovery rule |
|---|---|
| Local finished file / remote handoff | Local retained → PUT same identity/content → S3 object exists → register → confirmation permits local deletion. Lost PUT/register response leaves outcome uncertain and local data retained until reconciled. Registration may enable indexing before caller sees its reply. |
| Assignment record / live attempts | Eligible → atomic assignment/start time → temporarily ineligible → unfinished past retry threshold becomes eligible again. Old attempt may still run or win; expiry neither kills nor fences it. Completed remains ineligible, including after repeat registration. |
| Whole-file attempt | Assigned → input IO → document work/finalization with builders/scratch owned by the same task → output IO → publication. Actual scheduler/dependency conditions vary within this owner; function boundaries alone add no coordination states. Failure leaves unfinished file eligible for whole-file retry and possibly abandoned outputs. |
| Publication / competing attempts | All outputs ready (including zero-output case) → atomic whole-file outputs + completion. First commit wins, including an older attempt. Lost response requires winner reconciliation before deleting outputs. A loser preserves winner outputs and disposes only of its own safe-to-delete work. |
| Cleanup / drain | Confirmed registration authorizes local-input deletion; committed completion authorizes S3-input deletion. Late uploads, orphan inputs/outputs and ambiguous publication require repeatable discovery and safe disposal rules. Stop intake → finish/handoff accepted input before full drain. Cleanup ownership, retries and permanent-input errors remain to specify. |

Sources: [selected contracts](design_answers.md), [implementation gaps](implementation_status.md). The confirmed simplification is removal of stage mailboxes, shard-to-pipeline planning and incremental live-WAL fetch/truncation feedback. Remaining runtime and failure states must be inventoried from their implementations as they are added.

**Design requirements, distinguished from implementation:** one execution owner must retain responsibility for indexing resources through serialization, including any awaited CPU offload, without queues of detached builders. Budgets must follow surviving work. The implemented local core serializes synchronous writes/finishing; async offload and admission remain to build. Local framing/tail repair now exist. Input cleanup assumes metastore evidence plus repeatable object discovery/deletion; Janitor ownership remains a recommendation. Safe orphan detection and ambiguous handoff/publication reconciliation still need concrete protocols.

### Counting boundaries and avoiding double counting

- **P3** owns local record recovery and file finishing. **P6** owns handing accepted input to indexing, including proposed upload/registration and local deletion after handoff.
- **P4** owns ingest-shard placement. Ordinary request routing belongs to **P1**, so removing shards does not erase routing costs.
- **P5** owns assignment selection/delivery. **P7** owns current source-side incremental progress and assignment reset. **P9** owns durable publication eligibility/ordering. Proposed whole-file replay belongs to **P13**.
- **P8** owns initial indexing-stage execution handoffs, resource transfer, and runtime scheduling. **P3** owns storage locks/offload/cancellation; **P2** owns admission reservations. Semantic FIFO publication/reservation rules belong to **P9**, not a second copy in P8.
- **P10** owns input reclamation/orphan discovery after the indexing handoff. **P15** owns output upload/cleanup. P9 determines the winner; P15 must preserve that winner during deletion.
- **P13** owns attempt health, termination, and replacement. **P14** owns service-level readiness/draining. Shared merge execution has its own separate assessment.
- The actor diagnostic is another view of these same obligations. **Do not count its mechanisms twice.** Historical bugs establish particular interactions, not a numerical complexity penalty.

For every asynchronous handoff or persistent mutation, ask: failure before effect; success with lost response; crash after effect; retry while the original still runs; and which durable facts permit recovery. Count **different recovery rules**, rather than all locations where the same retry rule applies.

Also trace **resource-release dependencies**: what remains live, which owner/task can release it, what must be scheduled or admitted first, and whether release remains executable under pressure. Check each fallback's actual authority and upstream gate. Direct RPC and gossip do not provide independent recovery when both wait for the same blocked source handler. Liveness under shared locks/executors is part of the state and dependency inventory, not an optional performance appendix. A finite convoy or scheduling delay is enough to count this reasoning burden; proof of deadlock is unnecessary. Trace the interfaces explicitly while preserving ownership of each obligation.

## Responsibility comparison

The same 15 responsibilities remain below. Eliminated shard and stage protocols are distinguished from retained work, new durable handoffs and reduced API scope. Component/state evidence follows under indexing execution, alongside the detailed handoff traces.

| Responsibility | Current reasoning burden | Proposed reasoning burden |
|---|---|---|
| **P1 — Routing/results** | Generations, capacity, shard availability and partial outcomes. Gossip/CP/persist-response updates and ambiguous retries must preserve identity/results. Router chooses node, ingester chooses shard. [C1] | Reuses payload-forwarding router with same-AZ, index-affinity and self preferences. Partial outcomes and lost replies require per-subrequest retry reconciliation preserving accepted-record results. Explicit rejection of unsupported commit modes belongs here. Ordinary load balancing alone is no longer the selected design. |
| **P2 — Admission/validation** | Router permits, rate limits, mapper and WAL capacity. Router permits can end while detached destination requests retain payloads. WAL checks happen only after locking. Configurable pre-ack validation excludes transformed sources. [C2] | Reserve write-queue and local-storage capacity, including reserve for drain and finishing. Recover occupied/reserved capacity. Before admission, caller departure discards work. After admission, processing continues subject to validation/IO errors, as current persist already does. Charge buffers until release and disk until safe deletion. Executor ownership still needs implementation. Pre-ack validation and document errors remain. |
| **P3 — Local storage/recovery** | Logical queues, physical WAL, memory/positions and transferred log ownership. Global locks span CPU/IO executors, unrelated shards and control paths. Replay, partial effects and poison recovery couple cancellation with memory/disk consistency. Delayed Flush can acknowledge before kernel flush. [C3] | Per-index active/finished files and interrupted tails, with serialized write/finish ownership. Synchronous framing/repair and flush-before-ack are implemented; async IO integration remains open. Recover under stable IDs, finish old files and start fresh intake. Upload finished files unchanged: no shared-WAL extraction, queue replay or mixed-queue reclamation. Fail-closed writes require reopen after a partial mutation. Direct IO is optional. Fsync/host loss and volume lifetime remain separate. Upload/registration retry is P6. |
| **P4 — Ingest placement** | Metadata/model/local shards. `min_shards`, throughput-based scaling, placement and open/register/discover/replace/drain. Reconcile partial/ambiguous opens while established shards remain usable and old input remains drainable. The shard inventory also feeds pipeline sizing in P5. [C4] | Shard-placement protocol disappears. File creation remains P3, routing P1, file assignment P5, service scaling/draining P14. |
| **P5 — Assignment** | Desired/applied/gossiped plans, identities/fingerprints. Shard inventory → estimated CPU load → node placement/grouping → pipeline count → plan application/reapplication. Drift and partial-spawn repair must preserve assignment order versus acquisition/publication. Load thresholds, capacity, prior plans and packing couple ingest tuning to actor/memory lifetimes. [C5] | Ready/processing/completed eligibility, conditional assignment at one authority, timeout/lost-response rediscovery and atomic eligibility/start update. Workers take whole files within a resource-limited concurrency budget, independently of ingest shard counts. Assignment limits duplicates, grants no exclusive publication right, and needs no renewal/fencing. |
| **P6 — Input handoff** | Cursors/subscriptions/EOF, server tasks/fan-in and reconnect from forwarded position, distinct from durable published progress. Fetch also shares the live WAL lock. [C6] | Upload the finished file, register its stable identity, then delete locally. Retry or check recorded state after a lost reply. Registration confirmation authorizes deletion. This small ordered protocol replaces the live fetch/subscription/cursor machinery. |
| **P7 — Incremental progress** | Assignment status, read/checkpoint positions, locks/tokens, acquisition/reset, revocation/discard of unfinished pipeline work, and mixed-shard progress/output eligibility. [C7] | Whole-file processing removes this protocol. Completion remains P9, whole-file replay/overlap P13, connector-specific progress in external producers. |
| **P8 — Indexing execution and resource ownership** | Eight stages and supervisor, nine mailboxes, seven adjacent handoffs, plus feedback. Lifecycle, backpressure, scheduling, IO/permit waits and retained resources form interacting state variables. Writer/permit/delivery/execution lifetimes differ. Cleanup spans owners, and safe admission depends on memory release by later scheduled work. Each edge is traced below. | One task owns each attempt through indexing and memory release. Local transient work is dropped/rebuilt as a unit. Any resource reservation must cover surviving CPU work and results until release. Awaited CPU offload must preserve this requirement; executor/admission wiring is still unimplemented. Resource waits remain; independently queued builders would reintroduce stage state machines. Storage offload is P3, whole-file retry P13, durable outputs P15. |
| **P9 — Publication** | Split/checkpoint/token states, acquisition/FIFO ordering, ambiguous commit/token recovery, atomic visibility/progress and local revocation/metastore fencing. Overlap rejected; forward gaps accepted with warning. [C8] | One conditional metastore transaction publishes all outputs and completes the file, including zero-output completion. The first commit wins. Other attempts observe completion. A lost reply requires checking whether this attempt supplied the winner's outputs. No worker-to-worker arbitration or publication fencing is needed. Deletion remains P15. |
| **P10 — Input reclamation** | Positions/gossip/model/EOF/WAL views, delivery paths, missed updates, publication and shared-file deletion invariants. RPC/gossip share the Source handler **and then the ingester lock**. Reset can return stale CP positions. Direct EOF preserves identity; gossip deletes. [C9] | Find present input objects whose IDs are completed and delete them. Repeat the reconciliation after failed deletes or late uploads. Retain completion evidence so repeated registration cannot recreate work. Unregistered uploads need a safe-disposal predicate because registration may still be in flight. Janitor ownership remains an implementation choice. Local deletion after registration is P6. |
| **P11 — Publication waits** | Request targets, persist response versus position events, retry/disconnect/missed progress, coverage of every requested position/Force marker. Force commits indexing, not fsync. [C10] | **WaitFor and Force are explicitly unsupported/deferred.** This is reduced API scope. Supporting them later requires file-target tracking, forced file finishing/processing and completion reconciliation. |
| **P12 — Configuration/lifecycle** | Incarnations, mapper/fingerprint/source state, distributed convergence, reload/replan/cleanup, old work versus config/deletion. Invalid indexing docs can be skipped while advancing progress. [C11] | Index incarnations, pending files and terminal dispositions, conditional eligibility/publication checks, repeatable disposal after interruption. Old files must not bind to recreated indexes. Existing mapping behavior is retained without version pinning. Terminal disposal still needs specifying. Shared upload/cleanup recovery remains P6/P10/P15. |
| **P13 — Attempt supervision** | Eight stage lifecycles/protection/generation state, shared kill/nested supervision, partial joins/detached uploads/restart, old work versus replacement/assignments. [C12] | One supervisor per indexer process owns pipeline handles, observes errors/panics/unexpected exits and restarts with backoff. Drain/shutdown disables replacement. Idle pipelines are healthy. The selected restart policy and slot ownership contribute 5 states / 6 transitions; optional progress checks and active-task drain/join behavior remain unspecified. Restart restores processing capacity without changing input assignment/completion. Any pipeline recovers retry-eligible files, and older attempts can still win. This is planned, not implemented. Publication arbitration/output cleanup are P9/P15. [Supervision decision](design_answers.md#pipeline-supervision). |
| **P14 — Service draining** | Ready/retiring/decommissioning/outstanding queues, membership/placement/closure, closed-shard recovery, enough indexing capacity and working WAL/state gates to drain. [C13] | Accepting/draining/stopped plus outstanding files. Stop intake, await finishing/handoff, retry interrupted drain and retain acknowledged input until handoff. Routing elsewhere does not recover local-only input. Deployment/storage availability remain decisions. |
| **P15 — Output cleanup** | Staged/published/deletion-marked metadata/objects, store/metastore handoff, retry/GC, deletion versus visibility. [C14] | For a completed file, subtract the winner's outputs from its attempt-output inventory and delete the remainder. Repeating this diff covers late loser uploads. For unfinished files, preserve outputs still eligible for publication. Resolve a lost publication reply before treating this attempt as a loser. These are cleanup eligibility rules, not another coordination service. |

## Actors and mailboxes

Actors add queued ownership, concurrent lifetimes, priority scheduling, completion signals, progress attribution, and shutdown protocols. Consolidation removes contracts between initial indexing stages. Merge actors and service-level coordination remain.

```text
Source → DocProcessor → Indexer → IndexSerializer → Packager → Uploader
  ↑                                                             │
  └── SuggestTruncate ← Publisher ← Sequencer ← FIFO reservation ─┘
                          │             ↑
                          ↓        Proceed/Discard ← detached upload
                     MergePlanner
```

Source also sends `NewPublishLock` through DocProcessor to Indexer. The supervisor forwards assignments, observes health, and replaces failed generations. Capacities below are **low-priority message slots, not bytes**. Each mailbox also has an unbounded high-priority channel. Zero means rendezvous: sending waits for a receiver, not handler completion.

| Actor | Capacity | Incoming work | Runtime |
|---|---:|---|---|
| SourceActor | Unbounded | Loop, assignments, truncation | NonBlocking |
| DocProcessor | 10 | Raw batches, publication locks | Blocking |
| Indexer | 10 | Processed batches, locks, commit/resume | Blocking |
| IndexSerializer | 0 | Builders, empty checkpoint commits | Blocking |
| Packager | 1 | Serialized split batches | Blocking |
| Uploader | 0 | Packaged batches | Default/current |
| Sequencer | Index upload concurrency | Ordered upload-completion receivers | Default/current |
| Publisher | 1 | Split/checkpoint updates | Default/current |
| IndexingPipeline supervisor | Unbounded | Assignment, spawn, supervision | Default/current |

Evidence: [wiring](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L329), [source](quickwit/quickwit-indexing/src/source/mod.rs#L320), [processor](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L535), [indexer](quickwit/quickwit-indexing/src/actors/indexer.rs#L350), [serializer](quickwit/quickwit-indexing/src/actors/index_serializer.rs#L50), [packager](quickwit/quickwit-indexing/src/actors/packager.rs#L93), [uploader](quickwit/quickwit-indexing/src/actors/uploader.rs#L236), [sequencer](quickwit/quickwit-indexing/src/actors/sequencer.rs#L33), [publisher](quickwit/quickwit-indexing/src/actors/publisher.rs#L186), [defaults](quickwit/quickwit-actors/src/actor.rs#L118).

There are **8 stages + supervisor, 7 adjacent handoffs, 9 mailboxes / 18 priority channels**. With P fully instantiated initial pipelines, these become **9P actor instances and 18P channels**, before auxiliary tasks and overlapping generations. They exclude IndexingService, framework scheduling, source/RPC tasks, detached uploads and merges. Each retained merge pipeline adds six stage actors + supervisor, with a separate shared scheduler in the legacy graph. Active handlers, blocked senders and one displaced low-priority message can retain payloads outside queue capacity. [Priority implementation](quickwit/quickwit-actors/src/channel_with_priority.rs#L117)

Outside this count, the main interacting roles are request routing/admission, ingester persistence/fetch/maintenance, control-plane shard management, indexing plan scheduling, IndexingService plan application, metastore publication, object storage and cleanup. These are roles, sometimes colocated, rather than eight additional processes. P1–P15 describe their state and interactions. In ingesthree, routing, intake, registration, assignment, worker supervision, publication and cleanup remain. The control-plane shard lifecycle and persistent shard-to-pipeline plan disappear. A complete implementation-wide task/owner count remains to inventory.

### Lifecycle versus operational state

**Backpressure is an operational state even while `ActorState` says `Running`.** It is not a fifth mutually exclusive lifecycle variant. Lifecycle, execution condition, payload ownership and resource reservations are separate facts. This is where component count creates interacting state, rather than merely adding enum variants.

| State variable / scope | Values or conditions that need reasoning | Evidence / consequence |
|---|---|---|
| Framework lifecycle, per actor | **4 declared values:** Running, Paused, Success, Failure | [ActorState](quickwit/quickwit-actors/src/actor_state.rs#L19). Running does not imply useful progress. |
| Execution condition, per task/handler | Computing; waiting for input; **backpressured on downstream admission**; waiting for a permit; waiting for a lock; waiting for IO/RPC; runnable but awaiting scheduling | Review classes, not an exhaustive enum or reachable-state count. The handoff and WAL traces identify concrete instances. |
| Payload ownership, per batch/workbench | Blocked sender, mailbox, active handler, detached job or returned result; different batches can occupy these simultaneously | A bounded mailbox does not bound all live payloads. Admission completion differs from execution completion. |
| Capacity/resource lifetime, per owner | Reservation held/released versus buffer/writer/scratch still live; caller gone versus admitted work continuing | Premature permit return can admit more work before older memory is freed. |
| Indexing progress, per assigned shard | **5 declared values:** Active, Complete, Error, NotFound, ReachedEof, plus a position | [IndexingStatus](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L109). EOF received differs from all input published. |
| Shard protocol status, per metadata/view | **3 meaningful values:** Open, Unavailable, Closed, plus Unspecified sentinel | [ShardState](quickwit/quickwit-proto/protos/quickwit/ingest.proto#L89). Local, CP and router views can diverge. |
| Publication eligibility, per lock/batch | Alive/dead flag, separately from mutex ownership and metastore token identity | [PublishLock](quickwit/quickwit-indexing/src/models/publish_lock.rs#L21). Revocation does not immediately free every queued resource. |
| Output metadata, per split | **3 declared values:** Staged, Published, MarkedForDeletion, separately from object existence | [SplitState](quickwit/quickwit-metastore/src/split_metadata.rs#L372). Lost replies and late effects require reconciliation. |

**Concrete combined state:** Publisher has committed P. Source is still Running but backpressured sending later batch R. Its queued truncation suggestion cannot run, so WAL capacity remains occupied. **Another:** Indexer returned its cooperative permit, Serializer is awaiting scheduling with live writers, and another pipeline admits work. These combinations explain failure mechanisms that a lifecycle enum or the former capped state score missed. They are source-established possibilities, not new incident reproductions.

The ingesthree design gives an attempt one owner across input IO, document work, output IO and publication. Its exact scheduling/admission waits are not implemented. Sequential work has one execution owner instead of seven stage queues and independent stage lifecycles. Several attempts, worker supervision, local-file/S3/metastore state and retained merges still interact. The simplification removes particular combinations and feedback dependencies; it does not imply four total states or a state-free worker.

Default upload concurrency is 12. Indexing receives 6 with the legacy merge scheduler, otherwise 12. Sequencer uses that indexing value; legacy merge Uploader receives the full configured value. Index/merge semaphores are process-global `OnceLock`s. This is not a simple 6+6 budget. [Config](quickwit/quickwit-config/src/node_config/mod.rs#L236), [wiring](quickwit/quickwit-indexing/src/actors/indexing_service.rs#L345), [semaphores](quickwit/quickwit-indexing/src/actors/uploader.rs#L203)

### Inter-stage handoffs

Each edge adds an independently scheduled owner, an admission/completion distinction, a retained payload while waiting, and drop/failure/shutdown rules. Seven is the boundary count, not a total-state or complexity measure. P8 owns these contracts; FIFO publication remains P9 and generation recovery P13.

| Handoff | Ownership and failure dependency |
|---|---|
| Source → DocProcessor | Raw batch/checkpoint leaves source; blocked send retains it and prevents Source handling truncation/assignment. |
| DocProcessor → Indexer | Parsed documents and lock updates cross another queue; expansion increases retained bytes and ordering must preserve publication ownership. |
| Indexer → IndexSerializer | Live writers cross rendezvous; indexing permit can end before the receiver runs and releases their memory. |
| IndexSerializer → Packager | Serialized splits/scratch move onward; a blocked send retains output and accounting guards even after writers finalize. |
| Packager → Uploader | Packaged split files/scratch wait at rendezvous; upload admission backpressures packaging and upstream stages. |
| Uploader → Sequencer | Transfers a FIFO completion receiver before permit acquisition, rather than finished output; detached task must Proceed/Discard or sender loss fails sequencing. |
| Sequencer → Publisher | Completed split/checkpoint updates wait behind earlier reservations and bounded publication admission; ordered publication must survive failures. |

The one-task proposal removes these seven message protocols, not the processing, IO waits or resource budgets. Returning live resources between separately queued CPU jobs can recreate their lifetime problem. Sources are linked in the inventory above and the ownership/publication traces below.

### Interaction and shutdown contracts

| Mechanism | Current obligation | Effect of one-task/file processing |
|---|---|---|
| Priority | Scheduled self-messages are high priority; ordinary data, assignments, locks and truncation use ordinary traffic. Priority applies between handlers. Outer Paused receive takes only high priority; an already-entered drain loop can continue consuming ordinary messages after Pause. | Stage priorities disappear; CPU fairness and cancellation still matter. |
| Reply lifetime | `send_message().await` returns a reply receiver after admission; most senders drop it. This does not cancel processing. `ask` also awaits the reply, whose sender must be released on receiver destruction. | Function results replace stage replies. Remote effects can still outlive caller cancellation. |
| Control behind data | A blocked downstream send prevents the current handler from returning to assignments/truncation. Unbounded Source admission does not ensure responsiveness. | Internal stage waits disappear; worker admission/IO can still block control. |
| Protected waits | Sends, selected RPCs, permits and Sequencer completion waits count as healthy. Detached uploads have no individual stage health handle. | Removes delegated stage-health chains; useful-progress monitoring and operation deadlines remain. |
| Kill and join | Kill checks occur between handlers. A high-priority nudge does not interrupt a handler awaiting IO, a permit or another send. | Fewer joins, but CPU offload/remote effects need cancellation and lifetime rules. |
| Publication revocation | Source kills the old `PublishLock`, discards unfinished progress, and sends a new lock through Processor/Indexer. Batches retain their lock. | Whole-file first-wins replaces source-assignment revocation. Expiry does not revoke publication rights. |
| Finalization/drop | Success/quit can flush/forward; failure skips flushing. Mailbox reference counts and receiver destruction control queued resource drop. Stale commit timers carry workbench IDs. | Local ownership simplifies cleanup; resource reservations must outlive actual work. |

Sources: [scheduled messages](quickwit/quickwit-actors/src/actor_context.rs#L342), [dispatch](quickwit/quickwit-actors/src/spawn_builder.rs#L278), [envelope](quickwit/quickwit-actors/src/envelope.rs#L124), [send](quickwit/quickwit-actors/src/actor_context.rs#L224), [progress](quickwit/quickwit-common/src/progress.rs#L19), [kill/join](quickwit/quickwit-actors/src/actor_handle.rs#L221), [lock](quickwit/quickwit-indexing/src/models/publish_lock.rs#L21), [reset](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L380), [processor finalize](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L556), [indexer finalize](quickwit/quickwit-indexing/src/actors/indexer.rs#L452), [timers](quickwit/quickwit-indexing/src/actors/indexer.rs#L480), [mailbox lifetime](quickwit/quickwit-actors/src/mailbox.rs#L46).

**Ordered uploads:** Uploader reserves a FIFO Sequencer position **before** acquiring a process-global permit, then checks kill and spawns a detached task. The task stages metadata, uploads its splits sequentially, and fulfills a separate semantic `Proceed/Discard` oneshot. Its permit ends after delivering that update, potentially before publication. Later completed uploads cannot bypass an older reservation, even when the oldest awaits another pipeline's permit. Explicit `Discard` cancels an ordered position; unexpected sender drop fails Sequencer. Preparation/staging errors drop the sender; upload errors also kill the generation. Capacity and concurrency must agree. Independent files remove this FIFO protocol, while atomic complete-file publication and durable upload ambiguity remain. [Reservation](quickwit/quickwit-indexing/src/actors/uploader.rs#L103), [upload task](quickwit/quickwit-indexing/src/actors/uploader.rs#L261)

**Health versus replacement:** Uploader explicitly calls its protected semaphore wait invalid usage. If an underlying operation never resolves, permits and linked protected waits can remain healthy without useful progress. This is a conditional hazard, not proof that normal S3 slowness deadlocks. A failed/unhealthy stage kills the shared generation. Termination explicitly joins Source, Indexer, Packager, Uploader and Publisher, while Processor, Serializer, Sequencer and detached uploads are not explicitly joined there. Joined work can delay replacement; unjoined work can overlap it. Termination checks run every second; progress checks normally every 30 seconds and consume a flag, requiring one periodic probe owner. [Uploader comment](quickwit/quickwit-indexing/src/actors/uploader.rs#L280), [termination](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L475), [heartbeat](quickwit/quickwit-actors/src/lib.rs#L68), [single probe](quickwit/quickwit-actors/src/actor_handle.rs#L77)

**Lock nuance:** Publisher holds the lock mutex through publication/retry sleeps but releases it before truncation/merge feedback. Source is unbounded. This is not a lock-held bounded-feedback-mailbox cycle. A dead lock prevents publication without immediately discarding queued builders or uploads; Serializer has no dead-lock early exit. [Publication/retries](quickwit/quickwit-indexing/src/actors/publisher.rs#L153), [feedback](quickwit/quickwit-indexing/src/actors/publisher.rs#L237)

**Source nuance:** zero-delay loops self-send on Source's unbounded ordinary mailbox. The framework's bounded-self-send deadlock warning is a general API hazard, not evidence of deadlock in this wiring. [Loop](quickwit/quickwit-indexing/src/source/mod.rs#L386), [warning](quickwit/quickwit-actors/src/actor_context.rs#L310)

**External connectors:** Kafka has a blocking poll loop, 100-slot event queue, synchronous rebalance callbacks waiting for Source replies, and publication-driven offset commits. Downstream pressure can fill its queue, delay polls past `max.poll.interval.ms`, and cause eviction. Aborting its already-running `spawn_blocking` handle does not stop the closure; drop/exit conditions matter. These concerns move to external producers. At-least-once removes output-publication coupling, not polling, checkpoint persistence or partial-result recovery. [Callbacks](quickwit/quickwit-indexing/src/source/kafka_source.rs#L120), [queue](quickwit/quickwit-indexing/src/source/kafka_source.rs#L241), [poll warning](quickwit/quickwit-indexing/src/source/kafka_source.rs#L578), [finalize](quickwit/quickwit-indexing/src/source/kafka_source.rs#L515)

These actor mechanisms belong to P8 (queues, ownership and execution), P9/P15 (ordered publication and output recovery), P7/P10 (source feedback), P13 (health/replacement) and P2 (admission). They are another view of the same responsibilities. Retained merges are described separately below.

## WAL locks and runtime execution

The user's WAL-lock/runtime trouble is a first-hand report. The mechanisms below are established in the inspected baseline; actual incident durations, deployed revisions and the exact worker/lock wait chain remain unverified. **This is a separate node-wide progress dependency from the Source-handler and Serializer scheduling failures.**

### Critical sections and their scope

One `RwLock` covers the entire ingester WAL, and one mutex covers shard/state metadata. Full acquisition takes **WAL write, then state**. Partial acquisition takes state only. Fetch reads WAL without taking state. This ordering prevents an ABBA class; it does not limit duration. A full caller can already exclude fetch while still waiting for a partial holder to release state. Tokio's write-preferring queue also puts new readers behind a queued writer. [State/lock contract](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L44), [acquisition](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L332)

| Operation | Work performed while holding the guard | Consequence |
|---|---|---|
| Persist, full | Status/admission, all subrequest validation awaits, sequential appends, repair, positions and routing/capacity snapshot | A slow index stalls unrelated persists, fetch, truncate, capacity reporting and lifecycle operations. [Persist](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L412) |
| InitShards, full | Synchronous mapper construction plus every queue creation/fsync | Schema/metadata work excludes the data path across indexes. [Initialization](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L762) |
| Truncate/gossip/reset/retain, full | Entire multi-shard batches of sequential offloaded mutations, usage and terminal-drain checks | Release competes with intake on the same gate; maintenance can itself cause a convoy. [Truncate](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L821), [reset](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L290), [retain](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L959), [gossip](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L1121) |
| Fetch, WAL read | Synchronous in-memory iteration/copying; wrapped records can allocate; at least one record can exceed the nominal batch target | Occupies its async worker and delays writers. Guard is correctly dropped before channel send. [Fetch](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L129) |
| Capacity/debug, full | Usage/shard scans, tracker update or JSON construction | Read-like operations exclude fetch/writes; stale capacity and diagnostics can wait behind the lock they need to explain. [Capacity](quickwit/quickwit-ingest/src/ingest_v2/broadcast/capacity_score.rs#L58), [debug](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L869) |
| Open-fetch/close/idle/retire/decommission/shard snapshot, partial | State scans/mutations; status transitions await cluster update | Full callers can hold WAL while waiting for these state holders. Actual broadcast and retirement sleep occur outside the guard. [Status](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L73), [retire](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L993), [snapshot](quickwit/quickwit-ingest/src/ingest_v2/broadcast/local_shards.rs#L260) |
| Startup, raw full | Open/replay, reconstruct every shard, usage/readiness publication | Recovery retains both guards. Initializing callers are rejected, but replay still occupies an async worker. [Startup](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L228) |

### Lock exclusion and worker occupation are different mechanisms

Validation runs on the shared small-task Rayon pool while Persist keeps both guards. WAL mutations move the entire log into `spawn_blocking`, again with both guards retained. **Lock hold includes executor queue delay, execution and async rescheduling after completion**, not just disk latency. A delayed validation can therefore stop an ingester's fetch/reclamation before any disk append starts. Async lock/offload waits suspend the task; they do not themselves block a Tokio worker. [Validation](quickwit/quickwit-ingest/src/ingest_v2/doc_mapper.rs#L74), [offload](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L75)

Startup is different: `open_with_prefs` calls synchronous `MultiRecordLog::open_with_prefs` inside `tokio::spawn(async { ... })`, **not `spawn_blocking`**. Synchronous filesystem access/replay occupies that worker until the poll returns. Fetch copying, mapper construction and debug/usage scans also do synchronous CPU/allocation work on their caller's async worker. One occupied worker need not stall every task on a multi-worker runtime; on a single worker, unrelated timers/services cannot run meanwhile. [Open](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L56), [pinned replay](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L39)

Keep the execution pools distinct: main Tokio normally has `ceil(cpus/3)` workers (configurable); actor NonBlocking normally 1–2; actor Blocking its own finite worker pool; small-task Rayon `max(cpus/3, 2)`; and Tokio blocking pools serve offloaded IO. `tokio::spawn` inherits the active runtime, so local and network-served paths can differ. Indexing actors sharing Blocking are only one contention domain. [Main runtime](quickwit/quickwit-cli/src/main.rs#L33), [actor runtimes](quickwit/quickwit-common/src/runtimes.rs#L52), [Rayon](quickwit/quickwit-common/src/thread_pool/simple.rs#L99)

**Hidden synchronous storage work:** delayed Flush does not eliminate fsync. Queue create/delete force flush+fsync, physical rollover synchronizes file/directory, and truncation GC can fsync empty-queue positions then unlink files. These run inside the offloaded operation while full guards remain held. Partial truncation can adjust every remaining record's metadata; usage scans queues and debug summary can scan record metadata. An operation's cost is not just its returned bytes. [Queue operations](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L119), [truncate/GC](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L231), [rollover](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/rolling/directory.rs#L240)

### Cancellation, admission and recovery extend the dependency

`MultiRecordLogAsync` temporarily contains `None` while the blocking closure owns the log. The async continuation must restore it. Cancelling at that boundary or a task panic can leave it poisoned; subsequent access explicitly aborts the **process**. Ordinary IO errors normally return ownership. Production mutating RPCs deliberately use `OneTaskPerCallLayer` so caller timeout/disconnect does not cancel that unsafe interval. **Adding a timeout that drops the inner mutation is not a safe local fix.** [Poison/restore](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L35), [shield](quickwit/quickwit-common/src/tower/one_task_per_call_layer.rs#L27), [service wiring](quickwit/quickwit-serve/src/lib.rs#L1100)

That protection costs liveness and memory: router byte permits belong to the outer ingest future, while destination tasks survive timeout. A slow holder lets those tasks retain payloads waiting for the lock; a timed-out router returns its permit and can admit more work while the earlier persist can still append. Multiple routers share the destination. Its WAL/rate checks occur **after** acquisition, and its in-flight byte metric is a gauge, not pre-lock reservation. The WAL-full circuit breaker reacts to returned failures, not a request still waiting to check capacity. This establishes a budget-lifetime gap, not a measured unbounded-RSS incident. [Router budget](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L572), [RPC timeout](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L391), [destination metric](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L913), [breaker](quickwit/quickwit-serve/src/lib.rs#L1080)

WAL admission checks **used queue bytes**, not allocated capacity, retained append scratch, request/validation buffers, fetch copies or page cache. Truncation can reduce used bytes before allocations/RSS fall. Replay rebuilds all surviving queues without an intake-memory admission budget before Ready; reset waits for Ready. Restart is therefore not automatically a low-memory escape from backlog pressure. [Capacity](quickwit/quickwit-ingest/src/ingest_v2/mrecordlog_utils.rs#L123), [scratch/append](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L174), [usage](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L351)

**Second release gate:** even when Source handles truncation and delivers it successfully, RPC/gossip/reset all need the same full storage guard. A later persist paused in validation or IO can prevent reclaiming earlier published data, refreshing capacity or retiring the node. Correct lock order, admission-before-validation, fetch unlock-before-send, gossip no-op filtering and the reset RPC outside locks are useful existing mitigations. They do not bound the remaining critical sections or provide an independent reclamation lane.

**Instrumentation limits:** full acquisition combines WAL wait and state wait **after WAL is already held**; full hold starts only after both locks arrive. Completed acquire/drop histograms omit still-pending intervals and raw startup. The acquisition gauge lacks cancellation-safe decrement. Debug collection takes the contested lock. Measure outstanding holder/phase, separate WAL/state times, pool queue/run/resume delay and surviving request bytes without acquiring that same lock. [Metrics](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L477)

**Proposal comparison:** S3 removes streaming indexer fetch and publication-driven truncation from the live writer. One indexing task alone does not remove local write synchronization, validation, IO or cancellation safety. The selected per-index append files upload unchanged, removing shared-WAL extraction and mixed-queue reclamation. They allow isolated ownership but still need bounded queues, index-incarnation checks, tail recovery, flushing/finishing, descriptors and independently scheduled cleanup. Destination reservations are required to follow actual work after caller departure; recovery must have a defined memory envelope and execution pool. Page-cache bypass is optional and introduces no required protocol here. These remain implementation obligations, not assumed benefits.

## Serializer scheduling and OOM

**First-hand report:** the user experienced OOM because Serializer was not scheduled while its input remained resident. Current source establishes the following ownership/scheduling mechanism, consistent with that report. It does not reconstruct the incident's deployed configuration or allocation peak.

```text
build writers → move builders into message → release cooperative permit, if enabled
  → rendezvous matches → receiver scheduled → pre-handler yield → finalize writers
```

Moving a builder transfers ownership without releasing its Tantivy writer, directory, scratch, optional RAM directory or clustering tree. Memory remains in a pending send, matched async delivery, received envelope awaiting its pre-handler yield, or the handler's remaining builders. Send acceptance means none of those writers necessarily finalized. Indexer can then build another batch; another send can block retaining more writers. [Builder](quickwit/quickwit-indexing/src/models/indexed_split.rs#L34), [handoff](quickwit/quickwit-indexing/src/actors/indexer.rs#L625), [dispatch](quickwit/quickwit-actors/src/spawn_builder.rs#L278), [serializer](quickwit/quickwit-indexing/src/actors/index_serializer.rs#L79)

**Permit mismatch:** on drain, `end_of_work` releases the cooperative period before sending. Other commits destructure the workbench without retaining its period, dropping it before the awaited send. The message carries accounting guards, not the period. Even one permit can let pipeline B build while A's writers await serialization. Cooperative indexing defaults to false here; its deployment setting remains to verify. [Drain](quickwit/quickwit-indexing/src/actors/indexer.rs#L414), [period](quickwit/quickwit-indexing/src/actors/cooperative_indexing.rs#L129), [period end](quickwit/quickwit-indexing/src/actors/cooperative_indexing.rs#L204), [message](quickwit/quickwit-indexing/src/models/indexed_split.rs#L231), [default](quickwit/quickwit-config/src/node_config/mod.rs#L214)

**Dispatch competition:** Blocking is a finite multi-thread Tokio actor runtime, distinct from `spawn_blocking` and Rayon. Processor, Indexer, Serializer, Packager and MergeExecutor share it. Processor/Indexer disable per-message yielding and can drain ready messages while doing synchronous work. Serializer yields before handling. Long indexing, mapping, merge, IO or compression work can occupy workers; OS thread preemption does not switch Tokio tasks inside an executing poll. Protected receive/yield/send can still appear healthy. Temporary delay suffices for OOM when concurrent live allocations exceed headroom; permanent starvation is unnecessary. [Runtime](quickwit/quickwit-common/src/runtimes.rs#L52), [Processor](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L547), [Indexer](quickwit/quickwit-indexing/src/actors/indexer.rs#L397), [drain loop](quickwit/quickwit-actors/src/spawn_builder.rs#L308)

| Control | Actual bound/accounting | Exclusions |
|---|---|---|
| Heap threshold | Per workbench, checked after a batch | Overshoot, committed writers, other pipelines, serialization temporaries |
| Cooperative permit | Active workbench periods | Writers after commit, variable bytes per period, queues |
| Mailbox slots | Message count | Payload size/expansion, active handlers, blocked sends, matched deliveries |
| Writer/batch gauges | Ownership-lifetime estimates | No aggregate byte admission; some allocations omitted |
| WAL capacity | WAL intake memory/disk | Indexing, merges, caches and other process allocations |

Writer estimates include writer/RAM-directory usage but omit the separate clustering tree. Processed-batch gauges measure original input bytes, not parsed heap. Serializer's accounting guard can conversely survive through the Packager send after writers have finalized. Distinguish estimates, allocated/active/resident memory, RSS and file-cache accounting. There is no general `blocking_threads × heap_threshold` process bound. [Heap check](quickwit/quickwit-indexing/src/actors/indexer.rs#L581), [estimate](quickwit/quickwit-indexing/src/models/indexed_split.rs#L194), [processed bytes](quickwit/quickwit-indexing/src/models/processed_doc.rs#L43), [clusterer](quickwit/quickwit-indexing/src/docs_clustering/clusterer.rs#L66)

Pressure also retains upstream data: Serializer delay blocks Indexer, processed queues fill, Processor retains its output, raw queues fill, Source retains its batch, and fetch buffers/WAL remain live. A finite queue graph can exceed memory without a leak. Finalization consumes writers one by one, retaining the remainder and potentially allocating clustering/compression temporaries. Memory need not decrease monotonically from handler entry.

Current RAM finalization persists files and returns a disk-backed Index. Earlier post-serialization RAM retention was fixed by [#6904 / `40eb74e22`](https://github.com/quickwit-oss/quickwit/commit/40eb74e22555563cd67cc8df3c4c993d923cfbc2), already in this baseline. That differs from the reported pre-serialization scheduling failure. Scratch persists through packaging/upload; allocator residency can lag release. Tantivy can also own a compression thread/queue whose lifetime is not synchronously joined by outer-task drop. [Finalize](quickwit/quickwit-indexing/src/models/indexed_split.rs#L151), [writer config](quickwit/quickwit-indexing/src/actors/indexer.rs#L547), [compressor](https://github.com/quickwit-oss/tantivy/blob/e988ecb7a11109d89f9f4582b386089fc3a2fec0/src/store/store_compressor.rs#L159)

## Truncation blocked behind later input

**First-hand report:** truncation did not get through while Source was blocked by backpressure. In this checkout Publisher can enqueue to the unbounded Source mailbox, but Source cannot run the handler. Publisher drops the reply receiver and ignores send failure; its publication lock is already released. A different deployed version may have another admission bottleneck. Legacy ingest-v1 adds a Source→IngestApiService request with a three-slot mailbox. [Publisher](quickwit/quickwit-indexing/src/actors/publisher.rs#L133), [wiring](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L336), [legacy request](quickwit/quickwit-indexing/src/source/ingest_api_source.rs#L117), [legacy capacity](quickwit/quickwit-ingest/src/ingest_api_service.rs#L259)

A realistic schedule uses separate work:

1. Earlier commit **P** awaits its metastore publication response.
2. Later commit **Q** stalls downstream. Queues fill, and Source awaits DocProcessor admission for still later batch **R**.
3. P publishes; Publisher enqueues `SuggestTruncate(P)`.
4. Source cannot process it until R's send returns. P is searchable and checkpointed, while its WAL payload remains retained.
5. WAL limits reject new intake. Stopping producers does not finish R's pending send. Resuming downstream allows Source to return and process truncation.

Source's roughly one-second emit deadline bounds fetch accumulation, not the subsequent send. High-priority feedback would still wait for the current handler. An idle source whose handler returned can process feedback; a source with no new requests can still be blocked. [Emit/send](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L446), [handler](quickwit/quickwit-indexing/src/source/mod.rs#L508)

**Direct truncation and position gossip both start in that handler.** If metastore is at P but Source has not handled its suggestion, gossip, CP model and local truncation can all remain at older O. Multiple delivery paths provide independence only after the shared gate. [Both paths](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L295)

| Recovery path | Independence and limitation |
|---|---|
| Already-started RPC | Can progress; the RPC for P was never started. Delivery is best effort. |
| Already-emitted gossip | Can replay O; cannot invent un-emitted P. |
| `AdviseResetShards` | Runs outside Source, but reads the CP in-memory model, which can still return O. |
| `RetainShards` | Deletes removed/orphan queues; carries IDs, not fresh positions for retained queues. |
| Source acquisition/restart | Reads metastore P; requires working reacquisition/replacement. Protected waits/joins can obstruct it. |
| CP model reload | Can refresh from metastore; is a recovery event, not continuous fresh-position polling. |

Reset runs at ingester construction and is triggered by persist at disk usage ≥90%, subject to rate/concurrency limits. Memory-only pressure need not trigger it, and stopped requests do not keep triggering it. [Construction](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L261), [persist trigger](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L670), [model-based answer](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L1035), [acquisition](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L535)

**Delay versus deadlock:** backpressure→blocked Source→retained published WAL is established. A closed deadlock additionally needs downstream progress to require that same capacity's release. Ordinary intake rejection does not prevent fetching accepted records, so it alone does not close the cycle. Long IO/scheduling stalls can still cause outages. Protected sends can remain healthy; kill does not preempt them; termination joins Source before replacement. Classification requires the incident's deployed wait chain.

Even after handling the suggestion, release has further steps: Mrecordlog writes truncation metadata before memory eviction, so disk-full can obstruct cleanup; shared physical files remain until other queues release their records; direct EOF truncation preserves shard identity while EOF gossip drives deletion/draining; some helper IO failures log without propagating through RPC success. Verify actual positions/usage, not delivery alone. [WAL truncate](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L250), [local truncate](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L594), [direct EOF](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L811), [EOF gossip](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L1109)

## Resource-release requirements

For each retained resource, record live/admitted bytes, actual owner, execution pool, release trigger, resources needed to release, durable recovery authority, and fallback independence. This diagnostic informs P2/P8/P10/P13/P14 without additional points.

| Retained resource | Release prerequisite | Common failure dependency |
|---|---|---|
| Committed writers | Serializer finalizes/drops on shared runtime | Allocating work restarts after permit release before Serializer runs |
| Raw/processed batches | Downstream handles/drops them | Senders retain data while waiting to forward it |
| Published WAL | Source runs feedback, ingester obtains full guard and truncates | Source gate, then shared WAL/state gate; reset can also be stale |
| Timed-out persist payloads | Surviving destination task completes/drops | Router permit can return before detached work releases memory |
| WAL scratch/allocated capacity | Actual buffers shrink/drop | Used-byte admission can reopen before allocated memory/RSS falls |
| Recovery backlog | Replay completes before reset/readiness | Intake limits do not bound replay peak; synchronous open occupies async worker |
| Failed-generation allocations | Actual owner/CPU work ends | Replacement/permit lifetime ends before old allocations do, or kill stalls |
| Proposed local file | Confirmed registration, then local deletion | Full dispatch queue blocks the owner needed to handle older completion |
| Proposed completed S3 input | Durable completion discovery/deletion | Cleanup relies on transient notification or stalled original worker |

**One admitted owner through memory release:** continue indexing into memory-releasing serialization before returning the slot. Returning builders from one CPU closure and queuing another for serialization recreates the dependency. Reservations must follow queued/running CPU work and memory-owning results, including cancellation. The local `run_cpu_intensive` helper submits immediately, skips abandoned work only at its dequeue-time receiver check, and cannot interrupt a running closure. Cancelled captures stay live until dequeued. Its guidance targets <200ms jobs and recommends dedicated execution for long/high-share work. [Implementation](quickwit/quickwit-common/src/thread_pool/simple.rs#L73)

A worker-count limit bounds memory only with bounded per-attempt peaks and shared headroom. Whole-file atomic publication does not require whole-file RAM buffering. Account for parsing, serialization, compression, merges, co-located ingest and expired attempts. Resource control does not require changing first-publication-wins or adding assignment fencing.

**Independent draining:** proposed local deletion follows upload/registration, without waiting for indexing publication. Preserve that separation. Avoid `dispatch B waits on full upload queue → older A's completion waits for same owner → A cannot delete/release capacity`. Registration reconciliation, cleanup, file finishing, decommission and finishing recovered files must execute while intake is blocked and without another write. Reserve disk/memory for finishing/recovery and CPU/IO for handoff; the selected unchanged-file upload needs no extraction copy. Janitor should discover durable completion rather than depend only on notifications. External S3/metastore outages can still block drain; the requirement is independence from forward-work gates under stated dependency availability.

## Historical evidence

These fixes establish particular interaction hazards rather than proof of today's incidents. All commits in this table precede the baseline. Local Git established the changes; full linked GitHub discussions were not independently reviewed.

| Fix | Established behavior and qualification |
|---|---|
| [#2862 / `df1833d37`](https://github.com/quickwit-oss/quickwit/commit/df1833d37fb3130aadb97e7fa58821cb27c14fda) | Receiver destruction retained queued reply senders while senders lived, leaving `ask` pending. Fixed by draining. Deployment unspecified. |
| [#6636 / `9b022b53a`](https://github.com/quickwit-oss/quickwit/commit/9b022b53a7968bdda22ee2cd865f99ed3424b2b6) | Closed-source assignment could terminate supervisor between ticks. Simulation-cluster incident, proposed likely cause, deterministic regression. Recovery reportedly 2s→up to 30s. |
| [#6790 / `a8559881e`](https://github.com/quickwit-oss/quickwit/commit/a8559881ef851a073e14840c9391b0e8435cc6d4) | Hard-coded Sequencer capacity 2 imposed another upload limit. Now follows concurrency. Performance fix; old test deadlock comment is not a production RCA. |
| [#1642 / `c54362cd1`](https://github.com/quickwit-oss/quickwit/commit/c54362cd15985c37c3454dd3ce584e288a0fc329) | Sequencer needed protected completion waits to avoid progress timeouts. Environment unspecified. |
| [#4452 / `a2e3b92ba`](https://github.com/quickwit-oss/quickwit/commit/a2e3b92ba52b34c4619161e4960ab05420675413) | Paused indexer/high-priority processing plus queued low-priority work could busy-loop drain logic until Resume. |
| [#3742 / `d2afdbd40`](https://github.com/quickwit-oss/quickwit/commit/d2afdbd40ab1d6ccfc76ea53be37b2e87d4ad33d) | Longer heartbeat delayed respawn; shared Observe messages could start extra loops. Fixed supervision coupling. |
| [#3765 / `c4ca94a5d`](https://github.com/quickwit-oss/quickwit/commit/c4ca94a5d2e10a0598d3dd87df21adc5cf69e32f) | Observation refreshes accumulated and supervisors awaited timeouts. Added debounced nonwaiting refresh. |
| [#4251 / `9f20660e8`](https://github.com/quickwit-oss/quickwit/commit/9f20660e862dc4a675a7b95962b29a8f6b8f81e8) | Weak-mailbox upgrades missed reference counting; final enqueue/drop raced exit checks. Framework lifetime defects. |
| [#4653](https://github.com/quickwit-oss/quickwit/commit/05311f1cec7ebb8f992b50fdc61a7d329953742b), [#4762](https://github.com/quickwit-oss/quickwit/commit/90c17c65e850b8fc558c84f4dad8f9ea8eafb029), [#4874](https://github.com/quickwit-oss/quickwit/commit/cfa219148379f8400684004262aedbd51eb3865c) | WAL offload documented cancellation poisoning; per-call detachment protected mutations; local-path layer coverage then needed fixing. History establishes ownership-safety machinery, not incident reconstruction. |
| [#6203](https://github.com/quickwit-oss/quickwit/commit/2bcade62927116643e313cef4d4260b2ee43b976) | Startup lock order aligned with WAL→state to prevent ABBA with capacity broadcast. Fixed in this baseline. |
| [#6613](https://github.com/quickwit-oss/quickwit/commit/4efe8cbb018f5d3b02706dfb019331f60bf99eaa), [#6383](https://github.com/quickwit-oss/quickwit/commit/7d80c939a23be5300ef828f439a1d47b3c0b8dcb) | Gossip prefilters unrelated/no-op changes before WAL write acquisition; operation-level lock telemetry improved. Mitigations for contention, not duration guarantees. |

Legacy ingest-v1 teardown-deadlock comments concern test shutdown, not current ingest-v2 production. [Example](quickwit/quickwit-indexing/src/source/ingest_api_source.rs#L349)

### Incident discussion and later changes

A [first-hand incident summary](https://dd.enterprise.slack.com/archives/C0C9PRLE9H6/p1792583837979529) separates initial OOM from crash loops, calls for a global indexing memory limit and inline Indexer/Serializer/Packager execution, and reports reproduction. That reproduction was by the incident author, not this analysis. The [simulation update](https://dd.enterprise.slack.com/archives/C0C9PRLE9H6/p1792169725567219) reports compressible-input/high-pipeline-count crash loops while leaving equivalence to production uncertain. An [earlier hardware explanation was withdrawn](https://dd.enterprise.slack.com/archives/C0C9PRLE9H6/p1792153037036229).

A [secondary, agent-authored telemetry analysis](https://dd.enterprise.slack.com/archives/C0C9BFA1Y6T/p1792144830962529) reports 27.540s before serialization and 14.420s before packaging, with packaging itself about 25ms. Its [caveats](https://dd.enterprise.slack.com/archives/C0C9BFA1Y6T/p1792144845087379) leave mailbox residence versus runtime dispatch unresolved and say publication continued. These traces were not independently queried here and do not establish CPU saturation or deadlock.

Related changes **outside the baseline**, with deployment/resolution unverified:

- [`cb47630e`](https://github.com/quickwit-oss/quickwit/commit/cb47630e67d40ee304c54312a746835fe12ff9b3): best-effort actor priority boosts Serializer against non-yielding stages; it cannot preempt running handlers.
- [`29e5f564a`](https://github.com/quickwit-oss/quickwit/commit/29e5f564ae2a445eff5e5376dd09dd8a1ec31c62): inline serialization/packaging retains the cooperative period through memory-heavy processing. [PR #6967 discussion](https://dd.enterprise.slack.com/archives/C0AG168HA6N/p1793612846293189) showed draft status.
- [`a20e2fdbe`](https://github.com/quickwit-oss/quickwit/commit/a20e2fdbe3bff7a8cbab6b29d1ac774fddb6051f): shared source-side memory throttle, a separate admission intervention rather than an atomic byte reservation.
- [`d9acd2df`](https://github.com/quickwit-oss/quickwit/commit/d9acd2df97d7459162187e0f6d972555babcdaf9) moves validation before locking with mapper rechecks; [`0163cc71`](https://github.com/quickwit-oss/quickwit/commit/0163cc712efe9d08c09163db9f842af0c45c6e2d) shards WAL ownership and adds in-flight append/close/position reconciliation. Shrinking locks requires replacement correctness rules.
- [`ad6d24e8`](https://github.com/quickwit-oss/quickwit/commit/ad6d24e8f3d6426a1f15faa00ebc503aaf03102e) offloads fetch in that later sharded-WAL design, whose synchronous per-instance mutex can occupy async workers waiting for append. **That mutex is absent from this baseline**; its WAL acquisition is async. Match the deployed revision before attributing a worker-blocking incident to either mechanism.

## Retained merge actors

The legacy graph remains `MergePlanner → shared MergeSchedulerService → MergeSplitDownloader → MergeExecutor → Packager → Uploader → Publisher → MergePlanner`, fed by initial publication notifications.

| Actor | Low-priority capacity | Retained interaction |
|---|---:|---|
| MergePlanner | 1 | Candidates, inventory, notifications, restart |
| Shared MergeSchedulerService | Unbounded, plus unbounded pending heap | Unsupervised scheduler, permits, fast planner replies |
| MergeSplitDownloader | Unbounded | Task-carried permits constrain actual work |
| MergeExecutor | 1 | CPU work, downstream waits |
| Merge Packager | 1 | Packaging/handoff |
| Merge Uploader | 0 | Detached tasks, global upload permits |
| Merge Publisher | Unbounded | Atomic replacement, planner feedback |
| MergePipeline supervisor | Unbounded | Six-stage reconstruction/health |

There are six stages + supervisor, plus shared scheduler; each mailbox also has high priority. [Handles](quickwit/quickwit-indexing/src/actors/merge_pipeline.rs#L75), [planner](quickwit/quickwit-indexing/src/actors/merge_planner.rs#L318), [scheduler](quickwit/quickwit-indexing/src/actors/merge_scheduler_service.rs#L108), [downloader](quickwit/quickwit-indexing/src/actors/merge_split_downloader.rs#L100), [executor](quickwit/quickwit-indexing/src/actors/merge_executor.rs#L156), [publisher](quickwit/quickwit-indexing/src/actors/publisher.rs#L198).

A [permit](quickwit/quickwit-indexing/src/actors/merge_scheduler_service.rs#L29) travels with each task through publication; drop releases capacity/signals scheduling and planner tracking. Scheduler dispatch uses `try_send_message`, and Publisher is unbounded, so the feedback graph alone does not prove deadlock. Merge uploads go directly to Publisher without Sequencer; early preparation/staging failures can log/drop instead of failing a sequencing receiver. [Upload path](quickwit/quickwit-indexing/src/actors/uploader.rs#L261)

Planner mailbox/inbox survives restart, requiring reconciliation of queued notifications, metastore snapshot and planner incarnation. Graceful shutdown disconnects Publisher→Planner feedback and finalizes planning to drain admitted merges. A file task can still block notifying the one-slot planner after publication; a crash requires durable rediscovery under the future ownership policy. [Startup](quickwit/quickwit-indexing/src/actors/merge_planner.rs#L95), [reconstruction](quickwit/quickwit-indexing/src/actors/merge_pipeline.rs#L430), [shutdown](quickwit/quickwit-indexing/src/actors/merge_pipeline.rs#L477)

## Shared and moved responsibilities

These responsibilities remain alongside the 15-row core comparison on **both** sides. Connector-specific work is distinct from the core ingest-shard progress protocol in P7.

| Responsibility | Current | ingesthree |
|---|---|---|
| Intrinsic parsing, indexing, serialization, packaging | Schema/partition/builder complexity; disposable outputs can be rebuilt | Retained. Stage handoffs are removed under P8, not the processing itself. |
| Existing merge execution when enabled | Six stage actors + supervisor per merge pipeline, with shared scheduler in the legacy graph; task-carried permits, replacement and recovery | Retained. Initial-indexing consolidation removes none of these actors. |
| External connector-specific delivery/progress | Broker assignment, callbacks, checkpoints and publication feedback inside Quickwit | Moved to external producers. At-least-once intake acknowledgment replaces publication feedback. Replay and accepted-prefix checkpointing remain required despite revoke/retry. Concrete connectors need separate review. |

Merge placement/discovery adaptation for reusable file workers is an **additional unresolved interface**. Retaining the merge machinery does not establish that integration will require no changes. A lost `NewSplits` notification after publication still needs eventual rediscovery. The current restart query uses node/source/pipeline-related placement assumptions. [C15]

### Separate the reasons for simplification

1. **Single-task consolidation** removes stage mailboxes, ownership transfers, and generation fanout. Keeping indexing and memory-releasing serialization under one execution owner also removes the inter-stage scheduling and ownership-transfer boundary. That resource-lifetime property must survive CPU offloading. It is a separate architectural choice from using S3.
2. **Whole-file recovery** removes incremental fetch/publication positions and allows independent files to publish out of order.
3. **Non-exclusive assignment** removes assignment renewal/revocation and publication fencing by assignment identity.
4. **Shared S3 files and self-assignment** decouple indexing placement from the ingester that accepted the data, after upload and registration. Adding indexers increases the number of independent pipelines; each assigns itself eligible files through the metastore and reads them from S3. Indexing scale-out requires no shard transfer or input-data rebalancing. Worker admission/lifecycle and S3/metastore capacity remain. For comparison, current v2 scale-out changes shard-to-pipeline plans and fetch assignments; it does not inherently move ingester WAL files. The file design adds upload/registration and document-file cleanup. This is the scale-out consequence of P4/P5, not another counted protocol. [Scale-out decision](design_answers.md#indexing-scale-out).
5. **Externalizing sources and choosing at-least-once** moves producer machinery and removes exactly-once publication coupling. This is partly a changed guarantee, rather than a same-contract implementation improvement.
6. **Decoupling ingest partitioning from indexing concurrency** removes `min_shards → shard inventory → load/placement/grouping → number of pipelines`. Current initial opening uses `min_shards`; active scaling also respects it, with idle exceptions. The scheduler includes closed shards needing drain, estimates per-shard CPU from throughput or a configured default, packs shards under pipeline load thresholds, and preserves/revises previous tasks. With variable shard load disabled and default estimates, full pipeline capacity is 4,000 CPU-millis, shard load is 1,000, and the maximum packing load is 3,200: **floor(3,200 / 1,000) = 3 shards per pipeline**, rather than per indexer node. A separate 1,200 threshold limits retention of underloaded pipelines, introducing hysteresis. Variable-load estimation changes the packing limit. A pipeline can own several shards: this is an indirect control loop, not a one-to-one conversion. Operators must reason about how an ingest setting and load estimates affect pipeline count, CPU/memory allocation, and plan changes that spawn, stop or reassign pipelines. In ingesthree, per-index input-file creation does not prescribe worker count; admitted file attempts consume the indexing concurrency budget. This reasoning burden belongs to P4/P5, with resource effects in P8/P13, not another additive category. [Initial opening](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L589), [scaling](quickwit/quickwit-control-plane/src/ingest/scaling_arbiter.rs#L71), [load and closed shards](quickwit/quickwit-control-plane/src/indexing_scheduler/mod.rs#L204), [packing thresholds](quickwit/quickwit-control-plane/src/indexing_scheduler/scheduling/mod.rs#L40), [full capacity](quickwit/quickwit-proto/src/indexing/mod.rs#L198), [pipeline grouping](quickwit/quickwit-control-plane/src/indexing_scheduler/scheduling/mod.rs#L211), [plan application](quickwit/quickwit-indexing/src/actors/indexing_service.rs#L658).

HTTP durability strengthens from delayed Flush to flush-before-ack for process crashes with surviving host/storage. Kafka/Kinesis-style publication deduplication weakens to at-least-once ingest. Current connectors have different guarantees; Pub/Sub's early acknowledgment is an explicit exception. Permanent local-storage loss before handoff remains outside the proposed contract. These differences must accompany the architecture comparison.

## States and failure surfaces

Do not model the proposal as only `Ready → Processing → Completed`. Independently relevant facts include local-file existence, finished contents, S3 input existence, registration, overlapping attempts, staged outputs, publication, and cleanup.

Count the named local states and directed state changes using the ledger rule above. Describe consequential combinations separately, without multiplying flags into a theoretical state-space number. This table captures consequential recovery classes identified in the review. It is not a complete state or failure inventory.

| Failure/interleaving | Current rule | Proposed rule / open obligation |
|---|---|---|
| Persistence succeeds, client loses response | Retry can duplicate input; mutating persist continues despite caller cancellation. | Same at-least-once ambiguity and continued admitted processing. Budgets follow surviving work; write failures must not invalidate later acknowledged records. |
| Process crashes after successful acknowledgment | Replay surviving WAL; userspace-buffered acknowledged records may be lost. | Recover every acknowledged record with surviving host/storage. Validate interrupted-write rules against actual format. |
| Input available remotely, handoff response lost | Reconnect fetch using transport cursor; full restart uses published position. | PUT/register may have committed. Reconcile stable file ID/content before local deletion. |
| Worker stops while assignment changes | Revoke old publication lock, reacquire and replay from durable progress. | Retry whole file after threshold; older attempt remains allowed to win. |
| Publication commits, reply lost | Read durable checkpoint/output state before resuming. | Read completion and winner evidence before disposing of this attempt's outputs. |
| Only part of a file's outputs uploaded | Current incremental commits can publish completed checkpoint ranges. | Publish none of this file until all required outputs and complete processing are established. |
| Serializer delayed while builders remain live | Writers survive rendezvous/delivery; cooperative permit already returned; other work can allocate. Aggregate live allocations can exceed memory. | Keep memory-heavy processing and finalization under one admitted owner; account for CPU jobs/results after cancellation. |
| Persist delayed in validation/IO | Whole-ingester guards exclude unrelated fetch, cleanup, capacity and lifecycle operations, even after the blocking closure finishes until its waiter resumes. | Synchronous local writer ownership is implemented. Async scheduling, maintenance and independent finished-file handoff/cleanup remain to build. |
| Router timeout while destination persists | Router permit returns, detached destination payload/operation survives; later append/retry can duplicate. | Charge destination reservations to actual surviving work, not only the requester. |
| Restart under backlog pressure | Synchronous open occupies async worker and rebuilds queues before reset can run; intake memory limit is not replay admission. | Stable recovered IDs and incomplete-tail repair are implemented with a fixed-size payload-scan buffer. Directory/file metadata still grows with file count. Recovery runs synchronously on its caller; offload/dedicated execution remains to integrate. |
| Source blocked forwarding later input | Earlier publication's suggestion can enqueue, but handler cannot initiate RPC/gossip; CP reset can return stale positions. | Local release after registration must run independently of intake dispatch and indexing; durable Janitor discovery for completed S3 input. |
| Input cleanup notification lost | Already-emitted RPC/gossip can progress; acquisition reads durable checkpoints. Retain/reset have narrower authority and freshness limits. | Discover completed inputs durably and retry deletion. Janitor mechanism to finalize. |
| Upload finishes after input deletion | Current orphan queue cleanup reconciles against metadata/model. | Completion prevents reindexing; late physical objects still require discovery/deletion. |
| Cleanup races an allowed publication | Staging/publication/GC transitions and grace periods must agree. | Age alone cannot prove an attempt has stopped. Conditional disposal/publication rules remain required. |
| Publication succeeds before merge notification | Merge restart reloads eligible published splits under existing placement. | Ensure durable rediscovery under future worker/merge ownership; notification alone is insufficient. |
| Index name deleted and recreated | UID checks separate incarnations; async cleanup converges. | Bind old files and delayed calls to old incarnation; terminal cleanup policy open. |

Two concrete reuse concerns deserve targeted tests:

- **Publication waits:** `PublishTracker` ignores events for unknown shards, registers targets after persist success, and ignores a second target for an already-tracked shard. This suggests early-notification hangs and repeated-shard target coverage hazards. These are static findings, not reproduced incidents. Existing asynchronous tests do not force the critical ordering. [C10]
- **Output GC:** it lists staged candidates, then calls a marking operation that accepts both staged and published splits. The primitives permit `list staged → publish → mark for deletion` unless another protocol excludes the race. Reusing them for competing file attempts requires an explicit safety argument/test. [C14]

## Validation needed before treating the design as implemented

| Priority | Test family | Required result |
|---|---|---|
| 1 | Small deterministic file/attempt model with delayed effects and lost responses | Completion terminal, one full winner, no winner deletion, local data retained until handoff. Enumerate two overlapping attempts first. |
| 1 | Real metastore concurrency tests | One assignment succeeds initially; either old or new attempt may win; completion/output set atomic; zero-output completion; repeated registration cannot resurrect work. |
| 1 | Real subprocess crashes and short-write/flush failures on selected storage | Every acknowledged record survives process failure with host/storage available, including file finishing and recovery interrupted again. |
| 1 | Ambiguous PUT/register/publish and late cleanup tests | Same identity/content on retry; distinguish own prior win; eventual discovery of objects created after deletion. Requires selected cleanup rules. |
| 2 | Actor regression tests for current reference | Deterministically exercise closed-source assignment, reply-sender drop, sequencing, kill during wait, and notification registration ordering. See existing coverage below. |
| 1 | WAL critical-section and runtime isolation | Pause validation, blocking dispatch/IO and continuation separately. Check unrelated-shard fetch/truncate, capacity/retire/debug. Contrast healthy timers during offload with synchronous replay on one worker; verify fetch unlocks before backpressure. |
| 1 | Persistence lifetime and recovery memory | Time out local/gRPC requests and track surviving bytes/permits. Subprocess-test cancellation/panic after WAL take. Delay create/delete/rollover syncs; inject partial truncate/GC errors; measure used/allocated/scratch/replay memory and actual release. |
| 1 | Resource release under stalled scheduling/backpressure | Stall serialization after handoff and trace writer/permit lifetimes. Publish earlier P while Source is blocked sending later R, verify queued feedback and stale CP reset, then release the blocker. |
| 1 | Resource independence and drain | Fill proposed intake/dispatch capacity and stop new requests; older files must still finish/register/delete. Stall indexing while handoff continues. CPU cancellation retains reservations until actual release; retries remain resource-limited. |
| 2 | Configuration/deletion and merge notification loss | Chosen policy produces a clear oracle; old incarnation cannot publish into new; published work remains discoverable after lost notification. |
| 2 | External producer crash/revoke/partial-result tests | Checkpoints never pass unresolved preceding data; replay duplicates are allowed. |

Resource tests should force a matched serializer handoff before handler entry, use one cooperative permit across two pipelines, stress expanding batches, and cancel CPU work before dequeue/during execution/after result production. Verify actual owners and reservations, not just finished-task counts. For truncation, hold CP at O with metastore at P, stop intake, and repeat with EOF/drain. Inject truncate disk-full/IO failures and lost registration/cleanup responses. Stall indexing and merge notification while proving independent input handoff.

Instrument builder commit/send/match/handler/finalize and publication/suggestion enqueue/source handler/event/RPC/local truncation separately. Backpressure duration is recorded after admission completes, so an indefinitely pending send may not yet contribute. Gauges estimate ownership rather than exact RSS. [Metric update](quickwit/quickwit-actors/src/mailbox.rs#L203)

### Existing coverage and limits

Inspected tests, **not run here**: [Sequencer order/discard](quickwit/quickwit-indexing/src/actors/sequencer.rs#L130), [queued payload/reply drop](quickwit/quickwit-actors/src/channel_with_priority.rs#L324), [dead-source assignment](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L1110), [lock death waiting for guard](quickwit/quickwit-indexing/src/models/publish_lock.rs#L102), [invalid-token termination](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L858), [publisher token handling](quickwit/quickwit-indexing/src/actors/publisher.rs#L582), [merge isolation](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L1003), [observation debounce](quickwit/quickwit-actors/src/actor_handle.rs#L421), and [root cause through shared kill](quickwit/quickwit-actors/src/tests.rs#L279).

Coverage qualifications: the dead-source test's low-priority-supervision comment differs from current high-priority scheduled messages; `test_indexing_pipeline_all_failures_handling` covers mapping failures rather than all interleavings; [failpoint tests](quickwit/quickwit-indexing/failpoints/mod.rs#L73) name `uploader:after` while the [actual point](quickwit/quickwit-indexing/src/actors/uploader.rs#L423) is `uploader:intask:after`; the [merge early-kill test](quickwit/quickwit-indexing/failpoints/mod.rs#L219) notes protected-execution limitations; and the [planner inheritance test](quickwit/quickwit-indexing/src/actors/merge_planner.rs#L687) does not fully construct the queued-old-message scenario its comments describe. Test names alone do not establish coverage.

Measure acknowledgment/search latency, useful bytes/sec, duplicate work, memory/scratch/backlog, and S3/metastore operations separately. Lower coordination complexity can still trade off latency, replay cost, or throughput.

As the distributed prototype is implemented, extend the local-core inventory with its actual spawned tasks, mailboxes, operational states, ownership transitions, durable mutations, recovery owners and cleanup paths against the same responsibilities. Record any recreated stage queues or new coordination authorities and their interactions. Component counts alone do not establish reachable-state counts.

## Coverage and remaining uncertainty

This assessment now traces request admission/cancellation, WAL locks/offload/physical IO, fetch/replay memory, all seven indexing handoffs, publication/reclamation gates, supervision, draining, and retained merge interactions. These are inspected mechanisms, not a claim of exhaustive failure coverage. Outstanding checks include actual deployed revisions/configuration, measured lock/poll/IO tails and allocation peaks, cancellation/error interleavings under real storage, every external connector, and the proposal's eventual task/lock/queue graph. The component inventory remains scoped rather than exhaustive. The owner/transition inventory includes failure/retry, scheduling, streaming, supervision and shared-storage contention. It describes application call boundaries rather than claiming every state inside the OS, SDKs and remote services. Distributed ingesthree execution remains a design, so its exact runtime graph cannot yet be source-established.

## Conclusion

**Prefer the proposed design for reducing ingest/indexing coordination, subject to finishing its durability, cleanup, lifecycle, and resource contracts.** The clearest actor gain is ownership of an attempt by one task, rather than seven queued transfers plus feedback and nested supervision.

The counted inventory records **300 current local-state types and 459 state-changing transitions**, versus **53 states and 64 transitions in the implemented/specified ingesthree subset**, including the later pipeline-supervision contract. The latter is a design inventory with incomplete runtime detail, rather than an equivalent-detail implementation ratio. Component counts and dependency evidence explain what the transition counts contain. Current initial indexing has nine actors and eighteen priority channels per fully instantiated pipeline. Backpressure, permits, scheduling, IO and resource ownership add state beyond the four framework lifecycle values. Shard scaling and packing determine how many such pipelines exist, creating another control loop. ingesthree removes stage queues, shard-to-pipeline planning and incremental progress feedback, while retaining worker admission, a short upload/register/delete handoff, conditional metastore publication and set-based cleanup. Continuing admitted writes after caller departure is existing behavior. Mapping pinning and mandatory direct IO are outside the requirements. Deferred WaitFor/Force reduce API scope. The comparison now uses actual state transitions and dependencies, including offloaded jobs and both sides of WAL streaming. The synthetic totals have been withdrawn.

The most important requirement exposed by the reported incidents is **that capacity-release and recovery work remain executable under pressure**. Finish memory-heavy work before returning its reservation. Keep accepted-input handoff/reclamation independent of forward admission and indexing. Remove the live-writer→indexer-fetch→publication→truncate dependency, and deliberately replace the global-lock/offload/cancellation protocol. Fewer actors help when they remove those dependencies; one indexing task alone does not establish storage or whole-node isolation.

Do not promise a percentage of incidents eliminated. Use this scorecard to track which protocols were removed, which moved elsewhere, and whether implementation adds back the interaction complexity it was intended to avoid.

## Code evidence

References point into the inspected checkout. Line numbers are anchors, not exhaustive ranges.

- [C1] [Router discovery/requests](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L98), [routing state](quickwit/quickwit-ingest/src/ingest_v2/routing_table.rs#L28), [ingester shard selection](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L425), [partial outcomes](quickwit/quickwit-ingest/src/ingest_v2/workbench.rs#L152).
- [C2] [Validation and admission](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L425), [capacity checks](quickwit/quickwit-ingest/src/ingest_v2/mrecordlog_utils.rs#L123), [transforms/validation](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L909).
- [C3] [Persistence/recovery](quickwit/quickwit-ingest/src/ingest_v2/state.rs#L228), [acknowledgment](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L589), [async WAL ownership](quickwit/quickwit-ingest/src/mrecordlog_async.rs#L31), [mutations survive caller cancellation](quickwit/quickwit-common/src/tower/one_task_per_call_layer.rs#L27), pinned [mrecordlog](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/multi_record_log.rs#L39) and [persistence policy](https://github.com/quickwit-oss/mrecordlog/blob/3b3562efea468a3a72a244b16d3c555db0ddc95b/src/persist_policy.rs#L62).
- [C4] [Opening](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L793), [rebalancing](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L1116), [uncertain transactions](quickwit/quickwit-control-plane/src/control_plane.rs#L487).
- [C5] [Plan reconciliation](quickwit/quickwit-control-plane/src/indexing_scheduler/mod.rs#L391), [ordered application](quickwit/quickwit-indexing/src/actors/indexing_service.rs#L638).
- [C6] [Server fetch](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L43), [retrying subscriptions](quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L257).
- [C7] [Source state](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L109), [reset](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L380), [acquisition](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L493).
- [C8] [Checkpoint/token transaction](quickwit/quickwit-metastore/src/metastore/postgres/metastore.rs#L286), [publication](quickwit/quickwit-metastore/src/metastore/postgres/metastore.rs#L777), [overlap/gap checks](quickwit/quickwit-metastore/src/checkpoint.rs#L285), [token acquisition](quickwit/quickwit-metastore/src/metastore/postgres/queries/shards/acquire.sql#L1), [publisher retries](quickwit/quickwit-indexing/src/actors/publisher.rs#L153).
- [C9] [Truncation](quickwit/quickwit-indexing/src/source/ingest/mod.rs#L295), [position propagation](quickwit/quickwit-indexing/src/models/shard_positions.rs#L130), [direct EOF behavior](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L814), [EOF gossip](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L1109), [retain](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L411), [reset advice](quickwit/quickwit-control-plane/src/ingest/ingest_controller.rs#L1035).
- [C10] [Tracker event handling](quickwit/quickwit-ingest/src/ingest_v2/publish_tracker.rs#L98), [target registration](quickwit/quickwit-ingest/src/ingest_v2/publish_tracker.rs#L126), [caller registration](quickwit/quickwit-ingest/src/ingest_v2/workbench.rs#L152), [wait lifetime](quickwit/quickwit-ingest/src/ingest_v2/router.rs#L597).
- [C11] [Lifecycle changes](quickwit/quickwit-control-plane/src/control_plane.rs#L605), [config checks](quickwit/quickwit-indexing/src/actors/indexing_service.rs#L260), [document errors](quickwit/quickwit-indexing/src/actors/doc_processor.rs#L467), [index deletion](quickwit/quickwit-index-management/src/index.rs#L200).
- [C12] [Health/restart](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L196), [termination joins](quickwit/quickwit-indexing/src/actors/indexing_pipeline.rs#L475), [actor/mailbox evidence](#actors-and-mailboxes).
- [C13] [Ingester decommission](quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L993), [draining indexer eligibility](quickwit/quickwit-control-plane/src/indexing_scheduler/mod.rs#L526).
- [C14] [Stage/upload](quickwit/quickwit-indexing/src/actors/uploader.rs#L348), [GC discovery](quickwit/quickwit-index-management/src/garbage_collection.rs#L115), [deletion marking](quickwit/quickwit-metastore/src/metastore/postgres/metastore.rs#L1072), [object-before-metadata deletion](quickwit/quickwit-index-management/src/garbage_collection.rs#L419).
- [C15] [Merge stage handles](quickwit/quickwit-indexing/src/actors/merge_pipeline.rs#L75), [merge rediscovery](quickwit/quickwit-indexing/src/actors/merge_pipeline.rs#L430), [post-publication notifications](quickwit/quickwit-indexing/src/actors/publisher.rs#L294).
