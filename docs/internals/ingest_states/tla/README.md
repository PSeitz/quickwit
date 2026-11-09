# Input reclamation comparison

`InputReclamation.tla` compares the state space of the same protocol slice for
HTTP ingest-v2 and ingesthree. It complements the handwritten local-state inventory
in [ingest_complexity.md](../../../../ingest_complexity.md). The comparison counts
reachable configurations, not local state names, TLC-generated successor visits,
or a percentage of implementation complexity. It is a protocol abstraction,
not a refinement proof or an exhaustive count of Rust implementation states.

## Common configuration

- One index, one or two ordered work units with the same abstract payloads.
  A v2 unit is a publication interval in one shard. An ingesthree unit is one
  finished input file. These units are not literal network batches or WAL segments.
- Start with all inputs local, flushed, and acknowledged. Finishing file contents
  is already complete. This starting point does not assert that v2 always flushes
  before acknowledgment. Startup, admission, and the durability-policy difference
  are outside this comparison.
- One processing/publication attempt per unit. Both paths use the same abstract
  output-production step and ordered publication send, commit, and observation.
  There is no worker-count, actor-stage, mailbox-capacity, or CPU scheduling model.
- Publication order is fixed to unit order on both sides. This is a selected
  execution subset. Ingesthree can also publish out of order or group several files.
  V2 metastore checkpoint rules are broader than this contiguous sequence.
- Requests and observations can be delayed arbitrarily. No crashes, retries,
  reassignment, competing publication, or storage loss are modeled. Withheld
  responses may stop progress forever. There is no fairness/liveness claim.
- Same observation granularity: remote upload/registration/publication effects
  remain separate from caller observations. Input reads and authorized deletion
  effects are atomic. V2 Source's additional forwarding boundary remains explicit.
- No symmetry reduction, state/action constraints, or verification-only history
  variables. State fields belonging only to the other protocol stay constant.

The v2 Source, Publisher, and feedback paths can progress independently. The
file uploader follows the implemented sequential handoff: upload one file, register
it, delete it locally, then select the next. Indexing may read a file once
registration commits, before the uploader observes the response. Remote cleanup
uses durable completion, independently of the publication response.

## Code and contract mapping

V2 paths were inspected at `a9f260bab`. File handoff uses the current working-tree
implementation. File publication and remote cleanup are design contracts, with
that distinction preserved in the comparison.

| Modeled boundary | Source or contract |
|---|---|
| V2 fetch/read → Source-held payload → downstream forward | [Source emit/forward](../../../../quickwit/quickwit-indexing/src/source/ingest/mod.rs#L448), [fetch releases WAL lock before sending](../../../../quickwit/quickwit-ingest/src/ingest_v2/fetch.rs#L129) |
| V2 Source handles suggestions only after its active handler returns | [SourceActor handlers](../../../../quickwit/quickwit-indexing/src/source/mod.rs#L385), [actor dispatch](../../../../quickwit/quickwit-actors/src/spawn_builder.rs#L279) |
| Publication commit → response → enqueue SuggestTruncate | [Publisher](../../../../quickwit/quickwit-indexing/src/actors/publisher.rs#L250), [enqueue](../../../../quickwit/quickwit-indexing/src/actors/publisher.rs#L133) |
| Source handles suggestion → emits position and launches direct RPC | [truncate](../../../../quickwit/quickwit-indexing/src/source/ingest/mod.rs#L295), [suggestion handler](../../../../quickwit/quickwit-indexing/src/source/ingest/mod.rs#L625) |
| Direct RPC and gossip apply positions after the same Source gate | [direct truncation](../../../../quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L811), [gossip truncation](../../../../quickwit/quickwit-ingest/src/ingest_v2/ingester.rs#L1039), [position coalescing](../../../../quickwit/quickwit-indexing/src/models/shard_positions.rs#L287) |
| File upload → registration → local deletion | [uploader](../../../../quickwit/quickwit-ingest/src/ingesthree/handoff.rs), [writer](../../../../quickwit/quickwit-ingest/src/ingesthree/write_queue.rs), [handoff contract](../../../../quickwit/quickwit-ingest/src/ingesthree/README.md#upload-handoff) |
| Atomic file publication/completion and completed-input deletion | [publication](../../../../design_answers.md#file-assignment-and-publication), [proposed input deletion](../../../../design_answers.md#input-deletion-proposal) |

Source holds at most one distinguished work unit between read and forward.
Delaying that forward represents an active handler, potentially waiting on
unmodeled downstream work. The real DocProcessor mailbox has ten slots. A
Source-held second unit in this model does **not** establish that two actual
batches fill that mailbox. All intermediate indexing queues are abstracted away.

V2 pending suggestions preserve FIFO order through increasing positions. Scheduling
Source's input loop relative to queued suggestions is abstracted. Direct requests
can execute out of order. Gossip coalesces to the latest Source-emitted
position; it is not an independent metastore poll. Successful truncation means
logical record removal, not physical WAL segment deletion. Failed truncation
acknowledgments, EOF/shard deletion, and mixed-shard segment GC are excluded.

## Results

Both fingerprint polynomials 0 and 1 completed with the same counts, all safety
properties passing, and zero states left to explore:

| Work units | V2 distinct states | ingesthree distinct states | V2 / ingesthree depth |
|---:|---:|---:|---:|
| 1 | 13 | 29 | 11 / 14 |
| 2 | 86 | 163 | 20 / 27 |

**The file protocol has more reachable configurations under this encoding.** Its
additional remote copy, upload/registration observations, and independently timed
local/remote deletion contribute intermediate states. Totals also depend on the
chosen action boundaries and ordered-publication restriction. This is a net
comparison, not a quantitative attribution to any single mechanism.

Local reclamation becomes possible before publication. The storage handoff removes
a dependency on indexing/publication while adding its own states. Fewer dependencies
need not mean fewer global configurations.

This model deliberately omits the v2 stage actors, shard assignment/control plane,
and supervision whose removal motivates the broader simplification. It therefore
supports a specific dependency comparison, rather than an overall ranking.
Likewise, counts from the separate ingesthree handoff/groups/lifecycle models have
different scopes and cannot be added to or compared directly with this table.

### Checked properties and witnesses

- Every acknowledged unit retains a local/remote input or committed output.
- Published outputs have been produced from read input.
- V2 local deletion follows publication. File local deletion follows registration
  and its confirmation. Registered unfinished files retain their remote input.
- Publication stays ordered and committed progress remains stable.
- Direct/gossip positions originate from Source processing a publication response.

The runner additionally checks these negated predicates for witness traces:

| Probe | Result |
|---|---|
| All inputs can be reclaimed after publication | Witness on both paths with two units |
| Local input can disappear before publication | Witness for files; forbidden on v2 |
| Publication feedback waits while Source holds later input | Witness for v2 with two units; absent with one unit |
| File publication occurs before the uploader observes registration success | Witness for files |
| Remote deletion occurs before the publisher observes publication success | Witness for files |

The feedback witness establishes a dependency, not a deadlock or a reproduction
of a deployed capacity incident. The runner also checks the absence of this state
on the file path as a structural sanity check: its Source variables stay constant.
That check is vacuous for file reclamation and establishes no immunity to other
resource waits. The positive early-local-reclamation witness separately exercises
the file protocol's independence from publication.

Temporary mutations are rejected: local deletion after upload alone violates
`LocalDeletionAuthorized`; deletion after registration commits but before its
response violates `LocalDeletionRequiresConfirmation`; making gossip read
publication directly violates `FeedbackAuthority`; publication without ready
output violates `PublishedOutputsExist`.

## Reproduce

Requires Python 3, Java, and the official TLA+ tools JAR. From the repository root:

```bash
export TLA_JAR="$HOME/.cache/tlaplus/tla2tools-1.8.0.jar"
python3 docs/internals/check_ingest_reclamation.py --probes --mutations
python3 docs/internals/check_ingest_reclamation.py --fingerprint 1 --probes --mutations
```

`--units 1 2 3` explores another workload size. The runner prints the model/JAR
hashes and keeps configurations, logs, witness traces, and TLC state storage in a
temporary directory outside the repository. Expected probe/mutation violations
are checked explicitly and treated as successful tests. An unexpected violation
fails the command. Search depth is diagnostic, not a complexity score.

Verified with Java 21, TLC revision `d8fb231`, two workers, 1 GiB heap, JAR SHA-256
`e1f11b75c4b5bc4478ba0ac5cb6ae28e40a01b0016bc33e0aa97da22dfdbcd7f`.
