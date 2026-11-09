# Ingest protocol models

These TLA+ models check ingesthree's handoff and lifecycle rules and its proposed
group assignment/publication protocol. The design contract is in
[design_answers.md](../../../../../design_answers.md), and the broader failure
inventory is in [validation.md](../../../../../validation.md).

| Model | Checked-in configuration | Focus |
|---|---|---|
| `IngestHandoff` | One input, two attempts, two splits per attempt | Flush/acknowledgment, handoff, competing publication, input/output cleanup, crashes and late requests |
| `IngestGroups` | Three inputs, overlapping `{A,B}` / `{B,C}` groups, a singleton retry, two splits per attempt | Atomic group publication, shared outputs, rejection on any completed input, recovery of leftovers |
| `IngestLifecycle` | Two recovered files and one new input | Recovery upload/register/local-delete gate, local finish versus handoff drain, cancellation and deletion ownership |

Rust implements local persistence, upload, idempotent registration, local deletion,
the startup handoff gate, and `WriteQueue::drain()`. Group assignment/publication,
indexing workers, and remote cleanup remain proposed. See the
[implementation documentation](../README.md).

Each model explores its configured finite identities without state/action
constraints. Attempts are single-use identities, not reusable worker slots.
These are separate safety models, **not a composed end-to-end proof or a
refinement proof of the Rust implementation**.

## Run

Requires Java and the official [TLA+ tools](https://github.com/tlaplus/tlaplus/releases).
For example, download the release outside the repository:

```bash
mkdir -p "$HOME/.cache/tlaplus"
gh release download v1.8.0 --repo tlaplus/tlaplus \
  --pattern tla2tools.jar --dir "$HOME/.cache/tlaplus"
export TLA_JAR="$HOME/.cache/tlaplus/tla2tools.jar"
```

From this directory:

```bash
check() {
  java -XX:+UseParallelGC -Xmx1g -cp "$TLA_JAR" tlc2.TLC \
    -workers 2 -fp 0 -noGenerateSpecTE -metadir "$(mktemp -d)" \
    -config "$1.cfg" "${2:-$1}.tla"
}
check IngestHandoff
check IngestGroups
check IngestGroupsZero IngestGroups
check IngestLifecycle
```

TLC should report `Model checking completed. No error has been found.`
State storage goes outside the repository. Increase the sets in the configuration
for larger explorations. `IngestHandoff` fixes one input in the specification;
use `IngestGroups` for multiple inputs. `Parts = {}` checks completion with zero
output splits. Each attempt has its own output keys, even when split names match.
The probe configurations described below deliberately fail and are separate from
these passing safety checks.

## Single-file handoff

### Transitions

- Flush the already-buffered input, deliver acknowledgment, and finish the file.
- Crash/recover the ingester while retaining flushed data on surviving storage.
- Send an immutable input upload, execute it, and observe/reconcile its result.
- Send registration, execute its idempotent transaction, and observe/reconcile it.
- Delete the local file after establishing registration or completion.
- Send assignment based on eligibility, then recheck eligibility at commit.
- Let the retry threshold pass without stopping the previous attempt.
- Read/process the whole input and upload output splits independently.
- Submit/retry publication, commit it, and observe/reconcile completion separately.
- Abandon an attempt on crash while allowing already-sent requests to execute.
- Delete completed inputs and only the outputs of confirmed losing attempts.

Scheduling can pause any attempt between actions. Remote commits do not update
caller knowledge. Withholding observation represents lost responses, and crashes
clear volatile knowledge. Requests that remain in flight can execute after a
caller crashes. Input and output PUT retries can arrive after cleanup and recreate
objects. Retained completion evidence still prevents those inputs being indexed
again, and cleanup can run again.

A metastore transaction is one atomic action. Publication replaces the complete
published output set and records completion together. The latest assignment gives
no publication priority. Assignment responses use the `assigned` history set to
represent accepted replies delayed across reassignment. This bookkeeping does not
require an assignment-history table in the implementation.

### Checked properties

| Property | Requirement |
|---|---|
| `TypeOK` | Every variable remains within its declared state domain. |
| `AckAfterFlush` | An observed acknowledgment follows a successful flush. |
| `AcknowledgedRecoverable` | Acknowledged input remains local, in S3, or completed. |
| `HandoffBeforeLocalDeletion` | Removing flushed local input requires metastore evidence. |
| `InputAvailable` | Registered unfinished work retains its S3 input. |
| `AtMostOnePublication` | At most one attempt commits a winning publication. |
| `CompletionRetained` | Completed work stays completed despite registration retries. |
| `PublicationAtomic` | Completion identifies an assigned winner and its entire output set. |
| `PublishedObjectsExist` | Every published output object still exists. |
| `CleanupAuthority` | A losing-attempt cleanup decision agrees with terminal completion. |
| `AssignmentRequiresEligibility` | Successful assignment checks eligibility at commit. |
| `CompletionStable` | Completion, winner, and published output set remain unchanged. |

Together, recoverability and publication properties require the winning output
objects to survive even after both input copies are deleted. `committed` records
successful publications as specification history, so a second winner remains
visible to the checker even if the implementation overwrites the winner field.

### Scope and assumptions

This is a **safety model**, with no fairness or eventual-progress claim. It permits
arbitrarily delayed operations, permanently abandoned attempts, and terminal
states. Deadlock checking is disabled for that reason. Attempt IDs are single-use.
The configured number bounds the total attempts for this input, not merely the
number of concurrent workers.

- One fixed input payload and index incarnation are implicit. Identity, immutable
  bytes in finished files, collision-free attempt output keys, and successful
  whole-file processing are assumptions. Byte framing, partial appends, checksum validation,
  and mapping policy require Rust tests.
- The starting point is one buffered input. File creation, intake admission,
  finishing multiple files, routing, and source checkpointing are outside this model.
- Only process failure is modeled. Host/kernel and local storage survive. A
  successful flush survives an ingester crash. S3 and metastore retain committed
  state. Host failures, volume loss, and backend consistency violations are outside
  this contract.
- One pending request per operation identity represents delayed identical requests.
  Explicit network queues, arbitrary request multiplicity, and request-loss/retry
  timing are abstracted. Upload completion and observation remain distinct for
  inputs. For outputs, observing all uploads is folded into publication submission.
- Expiry is a nondeterministic event, rather than a wall clock. Conditional
  assignment and overlap are checked. Oldest-first scheduling is outside the
  single-file model.
- All attempts produce the configured number of splits. Empty-output completion
  is a separate configuration. Indexing and output contents are abstract.
- Completion evidence is retained indefinitely. Input cleanup assumes the proposed
  completed-input collector. Each cleanup action represents an authorized deletion
  effect. Terminal completion makes a delayed authorization safe.
- Output cleanup before input completion is excluded. Integrating existing staged
  split GC requires modeling its race with publication. Resource budgets, index
  deletion/recreation, completion-record retirement, merge notification recovery,
  multipart uploads, and liveness remain separate work.

TLC checks the explored finite configuration, not every possible system size.
This model is not a proof that the Rust, filesystem, S3, or metastore implementation
satisfies its assumptions.

## Group assignment and publication

`IngestGroups.tla` starts with immutable inputs already registered for one index.
An attempt chooses a nonempty group once. Assignment rechecks every input's
eligibility at commit. Expiry allows reassignment while older attempts retain
publication rights. Assignment replies can arrive after reassignment.

Outputs belong to the entire group. Publication atomically checks **every** input,
publishes all output splits, and completes all contributing inputs. If any input
has another winner, the whole publication is rejected. A retry of an already
committed attempt preserves that result. One observed foreign winner establishes
conclusive group loss and permits cleanup of that attempt's outputs, including
shared splits. Unfinished inputs from a rejected group remain eligible after expiry.

Submission, remote commit, and result observation are separate. Output PUT retries
can arrive after cleanup. Paused attempts can be abandoned indefinitely while their
sent requests execute. The model abstracts crashes as pauses rather than explicitly
clearing volatile state. `IngestHandoff` separately exercises crash/knowledge loss.

| Property | Requirement |
|---|---|
| `AtMostOnePublication` | Each input belongs to at most one committed publication. |
| `GroupAtomicity` / `CompletionCorrespondence` | A committed group completes all its inputs and publishes exactly its full output set. Every completed input has a committed winner. |
| `PublishedObjectsExist` / `CleanupAuthority` | Cleanup preserves published objects and requires conclusive group loss. |
| `AssignmentChecks` / `AssignmentRequiresEligibility` | Attempts use allowed nonempty groups and assignment checks every input's eligibility at commit. |
| `WinnersStable` / `GroupsStable` | Completion is terminal and an attempt never changes its group. |

`IngestGroups.cfg` allows `{A,B}`, `{B,C}`, and a fresh singleton attempt choosing
`{A}` or `{C}`. Any attempt may start first. `IngestGroupsZero.cfg` checks the same
competition with no output splits. `AttemptGroups` can be replaced to explore
other finite group choices. Whole-file processing, unique immutable output keys,
and atomic durable transactions are assumptions. Oldest-first selection, byte/count
limits, merges, pre-loss output GC, and completion retirement remain outside scope.

## Startup, finish, and drain

`IngestLifecycle.tla` covers the **uploader-enabled** write queue. It starts with
recovered inputs on surviving local storage. Upload and registration commits are
separate from their observations. Each recovered file follows upload → registration
→ local deletion. Intake opens only after every recovered file has been registered,
deleted locally, and had its file charge released. Registration failures therefore
keep startup closed. This supersedes the earlier upload-only gate.
One successful valid append is represented by each fresh file identity.

`uploaded` and `knownRegistered` represent observed per-file stages, rather than
implementation ID sets or a deferred-registration queue. Cross-file scheduling is
abstracted: Rust hands off one recovered file at a time, while the model permits
interleavings between files that preserve each file's stage ordering.

Closing intake prevents new admission while queued writes finish. Handoff may
continue until local `finish()` stops it. `drain()` additionally waits for upload,
registration, local deletion, and release of file charges before reporting success.
Startup/drain cancellation and permanent errors stop progress without reporting
successful drain. Transient failures and retry delays are stuttering steps.
Remote requests may commit after stopping or crashing, and restart repeats handoff
under the same identities.

Local deletion is split into starting blocking work, removing the file, and
releasing its charge. Async cancellation must retain directory ownership until
that work ends. A process crash terminates the work; recovery recounts local files.

| Property | Requirement |
|---|---|
| `AckAfterFlush` / `AcknowledgedRecoverable` | Acknowledgments follow flush and acknowledged inputs remain local or remote. |
| `HandoffBeforeLocalDeletion` / `RegistrationAfterUpload` | Local deletion follows registration, which follows upload. |
| `StartupGate` / `AdmissionRequiresIntake` | Recovered files are registered, deleted locally, and uncharged before intake. |
| `RegistrationRequiresUpload` | Each new registration request follows an observed upload, including during startup. |
| `DrainComplete` | Successful drain leaves no queued/local/charged files, releases ownership, and has handed off its accepted inputs. |
| `LocalAccounting` | Every surviving local file remains charged. This is file-level coverage, not byte-budget verification. |
| `DirectoryOwnership` | Ownership outlives the modeled blocking deletion, including async cancellation. |

Append/flush and filesystem recovery are atomic abstractions here. Byte-level
recovery, blocking append/recovery cancellation, exclusion between competing
openers, FIFO request ordering, queue/disk byte limits and reservations require
Rust tests. A stopped worker may fail queued, unacknowledged work. The model does
not establish that every admitted request finishes after cancellation. Local-only
queues without an uploader are excluded. Remote input deletion/publication is
excluded here and checked separately by `IngestHandoff`. All models assume
surviving host/storage, immutable identities/bytes, and retained remote commits.
No model proves eventual drain, recovery, or cleanup.

## Verification results

The current passing configurations completed with no invariant or action-property
violation and zero states remaining in the queue:

| Configuration | Distinct states | Search depth | Fingerprint polynomials |
|---|---:|---:|---|
| `IngestHandoff.cfg` | 1,847,919 | 46 | 0 (rerun unchanged) |
| `IngestGroups.cfg` | 5,210,620 | 45 | 0, 57 |
| `IngestGroupsZero.cfg` | 23,716 | 25 | 0 |
| `IngestLifecycle.cfg` | 74,524 | 45 | 0, 1 |

The original handoff verification also passed with fingerprint 1 and additional
configurations: **59,439 distinct states** for two attempts and zero output splits,
and **15,523,743 distinct states** for three attempts with one output split each.
Those additional handoff configurations were not rerun for this update.

### Original handoff reachability and mutation checks

The original handoff verification checked temporary copies with deliberately
broken rules. These mutations and the handoff probes below were not rerun for this
update. They were not applied to the specification:

| Mutation | Detected violation |
|---|---|
| Allow acknowledgment before flush | `AckAfterFlush` |
| Delete local input without observing registration | `HandoffBeforeLocalDeletion` |
| Delete S3 input before completion | `InputAvailable` |
| Let every publication overwrite the winner | `AtMostOnePublication` |
| Let a winning attempt delete its outputs | `PublishedObjectsExist` |
| Reset the file to ready on every registration | `CompletionRetained` |
| Accept assignment without rechecking eligibility | `AssignmentRequiresEligibility` |

Reachability probes also produced traces with an older attempt winning after
reassignment, a commit whose caller has not observed completion, and a winning
attempt that has crashed. These probes check that important executions are
actually reachable, rather than excluded by the model.

To reproduce the older-winner probe in a temporary copy, add this operator before
the module's closing line, then add `INVARIANT NoOlderWinner` to its configuration:

```tla
NoOlderWinner == store.phase # "completed" \/ store.latest = store.winner
```

That check **should fail** and print the witness trace. The unchanged configuration
should pass. The other probes similarly negate their target state predicates.

### New reachability and mutation checks

The following probes **should fail**, printing witnesses rather than protocol
violations. Using the `check` function above:

```bash
check IngestGroupsRecovery IngestGroups
check IngestGroupsOlderWinner IngestGroups
```

- Recovery: `{B,C}` commits, `{A,B}` is rejected while `A` remains unfinished,
  then a singleton retry completes `A`.
- Older winner: `{A,B}` publishes after `{B,C}` has taken over the assignment for
  `B`. Latest assignment supplies no publication priority.

Lifecycle probes use the normal configuration plus one negated target invariant.
For example, from this directory:

```bash
probe_dir=$(mktemp -d)
cp IngestLifecycle.tla IngestLifecycle.cfg "$probe_dir/"
printf '\nINVARIANT NoDrainWithNewInput\n' >> "$probe_dir/IngestLifecycle.cfg"
(cd "$probe_dir"; check IngestLifecycle)
```

Repeat with `NoIntakeAfterRecovery`, `NoStartupCancellationDuringDeletion`, or
`NoDrainCancellationDuringDeletion`. All four produced witnesses: intake after
complete recovery handoff, successful drain including new input, and startup/drain
cancellation while deletion still owns the directory. The former
`NoIntakeBeforeRegistration` probe was removed with the upload-only startup gate.
These show reachable executions, not eventual-progress claims.

Temporary mutations of the new models also produced the expected violations:

| Model / broken rule | Detected violation |
|---|---|
| Groups: reject only when **all** inputs are completed, rather than any | `AtMostOnePublication` |
| Lifecycle: open intake after uploads alone (the previous gate) | `StartupGate` |
| Lifecycle: open intake after registration but before local deletion | `StartupGate` |
| Lifecycle: open intake after removal but before deletion work/accounting finishes | `StartupGate` |
| Lifecycle: report drain success while queued/local/charged files remain | `DrainComplete` |
| Lifecycle: release the directory during blocking deletion | `DirectoryOwnership` |
| Lifecycle: delete locally without confirmed registration | `HandoffBeforeLocalDeletion` |

Tool: release `v1.8.0`, reporting TLC revision `d8fb231`, Java 21. Current runs used
two workers and a 1 GiB heap. Original handoff runs used four workers and 2 GiB.
Checked JAR SHA-256:
`e1f11b75c4b5bc4478ba0ac5cb6ae28e40a01b0016bc33e0aa97da22dfdbcd7f`.
