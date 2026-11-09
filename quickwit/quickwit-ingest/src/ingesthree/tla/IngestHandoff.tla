--------------------------- MODULE IngestHandoff ---------------------------
EXTENDS Naturals, FiniteSets

\* One immutable input file. Attempts are unique attempt IDs, not reusable
\* worker slots. Parts names the output splits produced by EACH attempt.
CONSTANTS Attempts, Parts
ASSUME /\ IsFiniteSet(Attempts) /\ Attempts # {}
       /\ IsFiniteSet(Parts) /\ "none" \notin Attempts

Outputs(attempt) == {<<attempt, part>> : part \in Parts}
AllOutputs == Attempts \X Parts
Phases == {"idle", "waiting", "reading", "uploading", "publishing",
           "stopped", "finished"}

VARIABLES local, client, input, store, workers, requests,
          assigned, objects, published, committed

vars == <<local, client, input, store, workers, requests,
          assigned, objects, published, committed>>

\* assigned and committed retain history for checking/reply reconciliation.
\* They are specification bookkeeping, not proposed additional database tables.
Init ==
    /\ local = [up |-> TRUE, data |-> "buffered",
                 uploaded |-> FALSE, registered |-> FALSE]
    /\ client = [flushed |-> FALSE, acknowledged |-> FALSE]
    /\ input = [uploadPending |-> FALSE, registerPending |-> FALSE,
                 present |-> FALSE]
    /\ store = [phase |-> "absent", expired |-> FALSE,
                 latest |-> "none", winner |-> "none"]
    /\ workers = [attempt \in Attempts |->
                     [phase |-> "idle", outcome |-> "unknown"]]
    /\ requests = [assignments |-> {}, outputs |-> {}, publications |-> {}]
    /\ assigned = {}
    /\ objects = {}
    /\ published = {}
    /\ committed = {}

Flush ==
    /\ local.up /\ local.data = "buffered"
    /\ local' = [local EXCEPT !.data = "flushed"]
    /\ client' = [client EXCEPT !.flushed = TRUE]
    /\ UNCHANGED <<input, store, workers, requests, assigned,
                    objects, published, committed>>

\* Delivery of a flush-qualified acknowledgment can be arbitrarily delayed.
Acknowledge ==
    /\ client.flushed /\ ~client.acknowledged
    /\ client' = [client EXCEPT !.acknowledged = TRUE]
    /\ UNCHANGED <<local, input, store, workers, requests, assigned,
                    objects, published, committed>>

FinishFile ==
    /\ local.up /\ local.data = "flushed"
    /\ local' = [local EXCEPT !.data = "finished"]
    /\ UNCHANGED <<client, input, store, workers, requests, assigned,
                    objects, published, committed>>

\* Host and volume survive. Remote requests can still execute after a crash.
CrashIngester ==
    /\ local.up
    /\ local' = [up |-> FALSE,
                  data |-> IF local.data = "buffered" THEN "absent"
                           ELSE local.data,
                  uploaded |-> FALSE, registered |-> FALSE]
    /\ UNCHANGED <<client, input, store, workers, requests, assigned,
                    objects, published, committed>>

RecoverIngester ==
    /\ ~local.up
    /\ local' = [local EXCEPT !.up = TRUE,
                  !.data = IF @ = "flushed" THEN "finished" ELSE @]
    /\ UNCHANGED <<client, input, store, workers, requests, assigned,
                    objects, published, committed>>

SendInputUpload ==
    /\ local.up /\ local.data = "finished" /\ ~local.uploaded
    /\ ~input.uploadPending
    /\ input' = [input EXCEPT !.uploadPending = TRUE]
    /\ UNCHANGED <<local, client, store, workers, requests, assigned,
                    objects, published, committed>>

\* A request holds the immutable bytes. A late PUT may recreate a deleted
\* completed input. Completion evidence still prevents re-indexing it.
CommitInputUpload ==
    /\ input.uploadPending
    /\ input' = [input EXCEPT !.uploadPending = FALSE, !.present = TRUE]
    /\ UNCHANGED <<local, client, store, workers, requests, assigned,
                    objects, published, committed>>

\* Successful response or reconciliation against the immutable object.
ObserveInputUpload ==
    /\ local.up /\ local.data = "finished"
    /\ input.present /\ ~local.uploaded
    /\ local' = [local EXCEPT !.uploaded = TRUE]
    /\ UNCHANGED <<client, input, store, workers, requests, assigned,
                    objects, published, committed>>

SendRegistration ==
    /\ local.up /\ local.uploaded /\ ~local.registered
    /\ ~input.registerPending
    /\ input' = [input EXCEPT !.registerPending = TRUE]
    /\ UNCHANGED <<local, client, store, workers, requests, assigned,
                    objects, published, committed>>

\* Idempotent by file identity, including after completion and input deletion.
CommitRegistration ==
    /\ input.registerPending
    /\ input' = [input EXCEPT !.registerPending = FALSE]
    /\ store' = IF store.phase = "absent"
                 THEN [store EXCEPT !.phase = "ready"] ELSE store
    /\ UNCHANGED <<local, client, workers, requests, assigned,
                    objects, published, committed>>

ObserveRegistration ==
    /\ local.up /\ local.data = "finished" /\ ~local.registered
    /\ store.phase # "absent"
    /\ local' = [local EXCEPT !.registered = TRUE]
    /\ UNCHANGED <<client, input, store, workers, requests, assigned,
                    objects, published, committed>>

DeleteLocal ==
    /\ local.up /\ local.data = "finished" /\ local.registered
    /\ local' = [local EXCEPT !.data = "absent"]
    /\ UNCHANGED <<client, input, store, workers, requests, assigned,
                    objects, published, committed>>

Eligible == store.phase = "ready"
            \/ (store.phase = "processing" /\ store.expired)

SendAssignment(attempt) ==
    /\ workers[attempt].phase = "idle" /\ Eligible
    /\ workers' = [workers EXCEPT ![attempt].phase = "waiting"]
    /\ requests' = [requests EXCEPT
                      !.assignments = @ \cup {attempt}]
    /\ UNCHANGED <<local, client, input, store, assigned,
                    objects, published, committed>>

\* Eligibility is checked at the transaction, not at the preceding listing.
CommitAssignment(attempt) ==
    /\ attempt \in requests.assignments
    /\ requests' = [requests EXCEPT !.assignments = @ \ {attempt}]
    /\ store' = IF Eligible
                 THEN [store EXCEPT !.phase = "processing",
                        !.expired = FALSE, !.latest = attempt] ELSE store
    /\ assigned' = IF Eligible THEN assigned \cup {attempt} ELSE assigned
    /\ UNCHANGED <<local, client, input, workers, objects, published, committed>>

\* Delayed reply. Losing the reply can strand this attempt until another
\* attempt retries. An accepted old attempt can start even after reassignment.
ObserveAssignment(attempt) ==
    /\ workers[attempt].phase = "waiting"
    /\ attempt \notin requests.assignments
    /\ workers' = [workers EXCEPT ![attempt].phase =
                      IF attempt \in assigned THEN "reading" ELSE "stopped"]
    /\ UNCHANGED <<local, client, input, store, requests, assigned,
                    objects, published, committed>>

ExpireAssignment ==
    /\ store.phase = "processing" /\ ~store.expired
    /\ store' = [store EXCEPT !.expired = TRUE]
    /\ UNCHANGED <<local, client, input, workers, requests, assigned,
                    objects, published, committed>>

\* Whole-file read, integrity verification and processing are abstracted into
\* one step. Their internal byte-level correctness requires implementation tests.
ProcessInput(attempt) ==
    /\ workers[attempt].phase = "reading" /\ input.present
    /\ workers' = [workers EXCEPT ![attempt].phase = "uploading"]
    /\ UNCHANGED <<local, client, input, store, requests, assigned,
                    objects, published, committed>>

SendOutput(attempt, part) ==
    /\ workers[attempt].phase = "uploading"
    \* Identical PUT retries may remain in flight after publication or cleanup.
    /\ <<attempt, part>> \notin requests.outputs
    /\ requests' = [requests EXCEPT
                      !.outputs = @ \cup {<<attempt, part>>}]
    /\ UNCHANGED <<local, client, input, store, workers, assigned,
                    objects, published, committed>>

CommitOutput(output) ==
    /\ output \in requests.outputs
    /\ requests' = [requests EXCEPT !.outputs = @ \ {output}]
    /\ objects' = objects \cup {output}
    /\ UNCHANGED <<local, client, input, store, workers, assigned,
                    published, committed>>

\* Observing all uploads and sending publication are collapsed. Commit remains
\* a separate step, so crashes and competing publication can intervene.
SendPublication(attempt) ==
    /\ workers[attempt].phase = "uploading"
    /\ Outputs(attempt) \subseteq objects
    /\ workers' = [workers EXCEPT ![attempt].phase = "publishing"]
    /\ requests' = [requests EXCEPT !.publications = @ \cup {attempt}]
    /\ UNCHANGED <<local, client, input, store, assigned,
                    objects, published, committed>>

RetryPublication(attempt) ==
    /\ workers[attempt].phase = "publishing"
    /\ attempt \notin requests.publications
    /\ requests' = [requests EXCEPT !.publications = @ \cup {attempt}]
    /\ UNCHANGED <<local, client, input, store, workers, assigned,
                    objects, published, committed>>

\* No latest-assignment check: the first committing publication wins.
CommitPublication(attempt) ==
    /\ attempt \in requests.publications
    /\ requests' = [requests EXCEPT !.publications = @ \ {attempt}]
    /\ IF store.phase # "completed"
       THEN /\ store' = [store EXCEPT !.phase = "completed",
                          !.expired = FALSE, !.winner = attempt]
            /\ published' = Outputs(attempt)
            /\ committed' = committed \cup {attempt}
       ELSE UNCHANGED <<store, published, committed>>
    /\ UNCHANGED <<local, client, input, workers, assigned, objects>>

\* Observation/reconciliation is separate from commit, including after a crash.
\* A recovery/cleanup worker may resolve an abandoned attempt by its identity.
ObserveCompletion(attempt) ==
    /\ attempt \in assigned /\ store.phase = "completed"
    /\ workers[attempt].outcome = "unknown"
    /\ workers' = [workers EXCEPT ![attempt].phase = "finished",
                      ![attempt].outcome =
                         IF store.winner = attempt THEN "won" ELSE "lost"]
    /\ UNCHANGED <<local, client, input, store, requests, assigned,
                    objects, published, committed>>

\* No fairness: a live attempt may pause indefinitely without being cancelled.
\* Crash abandons this unique attempt ID. Already-sent operations survive.
CrashAttempt(attempt) ==
    /\ workers[attempt].phase \notin {"idle", "stopped"}
    /\ workers' = [workers EXCEPT
                      ![attempt] = [phase |-> "stopped", outcome |-> "unknown"]]
    /\ UNCHANGED <<local, client, input, store, requests, assigned,
                    objects, published, committed>>

\* Terminal completion authorizes deletion. A stale observation is safe
\* because completion evidence is retained forever in this protocol.
DeleteInput ==
    /\ store.phase = "completed" /\ input.present
    /\ input' = [input EXCEPT !.present = FALSE]
    /\ UNCHANGED <<local, client, store, workers, requests, assigned,
                    objects, published, committed>>

DeleteOutput(attempt, part) ==
    /\ workers[attempt].outcome = "lost"
    /\ <<attempt, part>> \in objects
    /\ objects' = objects \ {<<attempt, part>>}
    /\ UNCHANGED <<local, client, input, store, workers, requests, assigned,
                    published, committed>>

Next ==
    \/ Flush \/ Acknowledge \/ FinishFile \/ CrashIngester \/ RecoverIngester
    \/ SendInputUpload \/ CommitInputUpload \/ ObserveInputUpload
    \/ SendRegistration \/ CommitRegistration \/ ObserveRegistration
    \/ DeleteLocal \/ ExpireAssignment \/ DeleteInput
    \/ \E attempt \in Attempts:
           \/ SendAssignment(attempt) \/ CommitAssignment(attempt)
           \/ ObserveAssignment(attempt) \/ ProcessInput(attempt)
           \/ SendPublication(attempt) \/ RetryPublication(attempt)
           \/ CommitPublication(attempt)
           \/ ObserveCompletion(attempt) \/ CrashAttempt(attempt)
           \/ \E part \in Parts:
                  SendOutput(attempt, part) \/ DeleteOutput(attempt, part)
    \/ \E output \in AllOutputs: CommitOutput(output)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ local \in [up : BOOLEAN,
                   data : {"buffered", "flushed", "finished", "absent"},
                   uploaded : BOOLEAN, registered : BOOLEAN]
    /\ client \in [flushed : BOOLEAN, acknowledged : BOOLEAN]
    /\ input \in [uploadPending : BOOLEAN, registerPending : BOOLEAN,
                   present : BOOLEAN]
    /\ store \in [phase : {"absent", "ready", "processing", "completed"},
                   expired : BOOLEAN, latest : Attempts \cup {"none"},
                   winner : Attempts \cup {"none"}]
    /\ workers \in [Attempts -> [phase : Phases,
                                 outcome : {"unknown", "won", "lost"}]]
    /\ requests \in [assignments : SUBSET Attempts, outputs : SUBSET AllOutputs,
                      publications : SUBSET Attempts]
    /\ assigned \subseteq Attempts /\ committed \subseteq Attempts
    /\ objects \subseteq AllOutputs /\ published \subseteq AllOutputs

AckAfterFlush == client.acknowledged => client.flushed

AcknowledgedRecoverable ==
    client.acknowledged =>
        \/ local.data \in {"flushed", "finished"}
        \/ input.present
        \/ store.phase = "completed"

HandoffBeforeLocalDeletion ==
    (client.flushed /\ local.data = "absent") => store.phase # "absent"

InputAvailable ==
    store.phase \in {"ready", "processing"} => input.present

AtMostOnePublication == Cardinality(committed) <= 1

CompletionRetained == committed # {} => store.phase = "completed"

PublicationAtomic ==
    /\ IF store.phase = "completed"
       THEN /\ store.winner \in Attempts
            /\ committed = {store.winner}
            /\ published = Outputs(store.winner)
       ELSE /\ store.winner = "none" /\ published = {} /\ committed = {}
    /\ committed \subseteq assigned

PublishedObjectsExist == published \subseteq objects

CleanupAuthority ==
    \A attempt \in Attempts:
        workers[attempt].outcome = "lost" =>
            (store.phase = "completed" /\ store.winner # attempt)

\* Check transaction preconditions and terminality across adjacent states.
AssignmentRequiresEligibility == [][(assigned' # assigned) => Eligible]_vars

CompletionStable ==
    [][(store.phase = "completed") =>
        (store'.phase = "completed" /\ store'.winner = store.winner
         /\ published' = published)]_vars

=============================================================================
