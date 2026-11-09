--------------------------- MODULE IngestGroups ---------------------------
EXTENDS Naturals, FiniteSets

\* Several immutable, registered input files for ONE index. Each unique attempt
\* chooses a nonempty group once. Outputs belong to the entire group, never to
\* individual files. AttemptGroups bounds choices, not execution interleavings.
CONSTANTS Files, Attempts, Parts, AttemptGroups
ASSUME /\ IsFiniteSet(Files) /\ Files # {}
       /\ IsFiniteSet(Attempts) /\ Attempts # {} /\ "none" \notin Attempts
       /\ IsFiniteSet(Parts)
       /\ AttemptGroups \in [Attempts -> SUBSET (SUBSET Files)]
       /\ \A attempt \in Attempts:
              AttemptGroups[attempt] # {} /\ {} \notin AttemptGroups[attempt]

\* Configuration helper: competing {A,B}/{B,C}, with a fresh singleton retry
\* for either leftover. Attempts may start in ANY order, including retry first.
OverlapGroups ==
    [attempt \in Attempts |->
        CASE attempt = "ab" -> {{"A", "B"}}
          [] attempt = "bc" -> {{"B", "C"}}
          [] attempt = "retry" -> {{"A"}, {"C"}}]

Outputs(attempt) == {<<attempt, part>> : part \in Parts}
AllOutputs == Attempts \X Parts
Phases == {"idle", "assigning", "working", "publishing", "won", "lost"}

VARIABLES groups, phase, requests, assigned, latest, expired, winner,
          objects, published, committed, stranded
vars == <<groups, phase, requests, assigned, latest, expired, winner,
          objects, published, committed, stranded>>

\* assigned and committed are history for checking/reply reconciliation.
\* stranded remembers files unfinished when a competing group was rejected.
\* These are specification bookkeeping, not additional database tables.
Init ==
    /\ groups = [attempt \in Attempts |-> {}]
    /\ phase = [attempt \in Attempts |-> "idle"]
    /\ requests = [assignments |-> {}, outputs |-> {}, publications |-> {}]
    /\ assigned = {}
    /\ latest = [file \in Files |-> "none"]
    /\ expired = {}
    /\ winner = [file \in Files |-> "none"]
    /\ objects = {} /\ published = {} /\ committed = {} /\ stranded = {}

Completed == {file \in Files : winner[file] # "none"}
Eligible(file) == winner[file] = "none"
                  /\ (latest[file] = "none" \/ file \in expired)
GroupEligible(group) == \A file \in group: Eligible(file)

SendAssignment(attempt, group) ==
    /\ phase[attempt] = "idle" /\ GroupEligible(group)
    /\ groups' = [groups EXCEPT ![attempt] = group]
    /\ phase' = [phase EXCEPT ![attempt] = "assigning"]
    /\ requests' = [requests EXCEPT !.assignments = @ \cup {attempt}]
    /\ UNCHANGED <<assigned, latest, expired, winner, objects, published,
                    committed, stranded>>

\* Recheck every selected row at the remote transaction. Concurrent listings
\* may select the same files, but only eligible groups are assigned atomically.
CommitAssignment(attempt) ==
    /\ attempt \in requests.assignments
    /\ requests' = [requests EXCEPT !.assignments = @ \ {attempt}]
    /\ IF GroupEligible(groups[attempt])
       THEN /\ assigned' = assigned \cup {attempt}
            /\ latest' = [file \in Files |->
                   IF file \in groups[attempt] THEN attempt ELSE latest[file]]
            /\ expired' = expired \ groups[attempt]
       ELSE UNCHANGED <<assigned, latest, expired>>
    /\ UNCHANGED <<groups, phase, winner, objects, published, committed, stranded>>

\* A lost assignment reply may strand this attempt. A new identity can retry.
\* A delayed successful reply remains valid even after reassignment.
ObserveAssignment(attempt) ==
    /\ phase[attempt] = "assigning" /\ attempt \in assigned
    /\ phase' = [phase EXCEPT ![attempt] = "working"]
    /\ UNCHANGED <<groups, requests, assigned, latest, expired, winner,
                    objects, published, committed, stranded>>

\* Nondeterministic passage of the retry threshold, with one start time per
\* assignment. Reassigned rows use the new attempt's threshold. Old attempts
\* keep their publication rights. No wall-clock ordering/fairness is assumed.
ExpireAssignment(attempt) ==
    LET current == {file \in Files:
                        latest[file] = attempt /\ winner[file] = "none"}
    IN /\ current \ expired # {}
       /\ expired' = expired \cup current
       /\ UNCHANGED <<groups, phase, requests, assigned, latest, winner,
                       objects, published, committed, stranded>>

\* Whole-file reading/processing is abstract. Immutable outputs have disjoint
\* attempt-scoped keys. A duplicate PUT can remain pending after observation,
\* publication, or cleanup, and can recreate a deleted losing output.
SendOutput(attempt, part) ==
    /\ phase[attempt] = "working"
    /\ <<attempt, part>> \notin requests.outputs
    /\ requests' = [requests EXCEPT
                      !.outputs = @ \cup {<<attempt, part>>}]
    /\ UNCHANGED <<groups, phase, assigned, latest, expired, winner,
                    objects, published, committed, stranded>>

CommitOutput(output) ==
    /\ output \in requests.outputs
    /\ requests' = [requests EXCEPT !.outputs = @ \ {output}]
    /\ objects' = objects \cup {output}
    /\ UNCHANGED <<groups, phase, assigned, latest, expired, winner,
                    published, committed, stranded>>

\* Observe all output uploads and submit publication. Remote commit remains
\* separate. An empty output set also completes the ENTIRE nonempty group.
SendPublication(attempt) ==
    /\ phase[attempt] = "working" /\ Outputs(attempt) \subseteq objects
    /\ phase' = [phase EXCEPT ![attempt] = "publishing"]
    /\ requests' = [requests EXCEPT !.publications = @ \cup {attempt}]
    /\ UNCHANGED <<groups, assigned, latest, expired, winner, objects,
                    published, committed, stranded>>

RetryPublication(attempt) ==
    /\ phase[attempt] = "publishing" /\ attempt \notin requests.publications
    /\ requests' = [requests EXCEPT !.publications = @ \cup {attempt}]
    /\ UNCHANGED <<groups, phase, assigned, latest, expired, winner,
                    objects, published, committed, stranded>>

\* First COMMIT wins, regardless of latest assignment. Reject the WHOLE group
\* if ANY input is completed. Reconciliation/retry of an already committed
\* attempt is idempotent. Durable completion is never retired in this model.
CommitPublication(attempt) ==
    /\ attempt \in requests.publications
    /\ requests' = [requests EXCEPT !.publications = @ \ {attempt}]
    /\ IF attempt \in committed
       THEN UNCHANGED <<winner, published, committed, stranded>>
       ELSE IF groups[attempt] \cap Completed = {}
            THEN /\ winner' = [file \in Files |->
                       IF file \in groups[attempt] THEN attempt ELSE winner[file]]
                 /\ published' = published \cup Outputs(attempt)
                 /\ committed' = committed \cup {attempt}
                 /\ UNCHANGED stranded
            ELSE /\ stranded' = stranded \cup (groups[attempt] \ Completed)
                 /\ UNCHANGED <<winner, published, committed>>
    /\ UNCHANGED <<groups, phase, assigned, latest, expired, objects>>

\* Response delivery or reconciliation is independent of remote execution.
\* One foreign winner is conclusive loss even while other inputs are unfinished.
\* A recovery worker may resolve an abandoned attempt before its request runs.
ObserveResult(attempt) ==
    /\ attempt \in assigned /\ phase[attempt] \notin {"won", "lost"}
    /\ IF attempt \in committed
       THEN phase' = [phase EXCEPT ![attempt] = "won"]
       ELSE /\ \E file \in groups[attempt]:
                    winner[file] \notin {"none", attempt}
            /\ phase' = [phase EXCEPT ![attempt] = "lost"]
    /\ UNCHANGED <<groups, requests, assigned, latest, expired, winner,
                    objects, published, committed, stranded>>

\* Shared outputs are deleted only after observing a definitive group loss.
\* This authority remains valid for delayed DELETEs and late output PUTs.
DeleteOutput(attempt, part) ==
    /\ phase[attempt] = "lost" /\ <<attempt, part>> \in objects
    /\ objects' = objects \ {<<attempt, part>>}
    /\ UNCHANGED <<groups, phase, requests, assigned, latest, expired, winner,
                    published, committed, stranded>>

Next ==
    \/ \E attempt \in Attempts:
           \/ \E group \in AttemptGroups[attempt]: SendAssignment(attempt, group)
           \/ CommitAssignment(attempt) \/ ObserveAssignment(attempt)
           \/ ExpireAssignment(attempt)
           \/ SendPublication(attempt) \/ RetryPublication(attempt)
           \/ CommitPublication(attempt) \/ ObserveResult(attempt)
           \/ \E part \in Parts:
                  SendOutput(attempt, part) \/ DeleteOutput(attempt, part)
    \/ \E output \in AllOutputs: CommitOutput(output)

\* Safety only: withholding local steps abstracts a stopped/crashed attempt;
\* already-sent remote operations and recovery observations can still execute.
\* No restart reuses an ID, and no fairness or eventual cleanup is claimed.
\* There are NO state/action constraints. Bounds are the configured finite
\* identities, possible groups, and output parts. Input lifecycle, oldest-first
\* scheduling, storage failure, GC before conclusive loss, and merges are outside
\* scope. Pauses abstract crashes without modeling lost volatile knowledge.
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ groups \in [Attempts -> SUBSET Files]
    /\ phase \in [Attempts -> Phases]
    /\ requests \in [assignments : SUBSET Attempts, outputs : SUBSET AllOutputs,
                      publications : SUBSET Attempts]
    /\ assigned \subseteq Attempts /\ committed \subseteq Attempts
    /\ latest \in [Files -> Attempts \cup {"none"}]
    /\ expired \subseteq Files
    /\ winner \in [Files -> Attempts \cup {"none"}]
    /\ objects \subseteq AllOutputs /\ published \subseteq AllOutputs
    /\ stranded \subseteq Files

AtMostOnePublication ==
    \A file \in Files:
        Cardinality({attempt \in committed: file \in groups[attempt]}) <= 1

GroupAtomicity ==
    /\ committed \subseteq assigned
    /\ \A attempt \in committed:
           \A file \in groups[attempt]: winner[file] = attempt
    /\ published = UNION {Outputs(attempt): attempt \in committed}

CompletionCorrespondence ==
    \A file \in Files:
        /\ (winner[file] # "none") <=>
               (\E attempt \in committed: file \in groups[attempt])
        /\ winner[file] # "none" =>
               (winner[file] \in committed /\ file \in groups[winner[file]])

PublishedObjectsExist == published \subseteq objects

CleanupAuthority ==
    \A attempt \in Attempts:
        phase[attempt] = "lost" =>
            /\ attempt \notin committed
            /\ \E file \in groups[attempt]: winner[file] \notin {"none", attempt}
            /\ Outputs(attempt) \cap published = {}

AssignmentChecks ==
    /\ \A attempt \in Attempts:
           /\ groups[attempt] # {} => groups[attempt] \in AttemptGroups[attempt]
           /\ attempt \in assigned => groups[attempt] # {}
           /\ phase[attempt] \in {"working", "publishing", "won", "lost"} =>
                  attempt \in assigned
    /\ \A file \in Files:
           latest[file] # "none" =>
               (latest[file] \in assigned /\ file \in groups[latest[file]])

AssignmentRequiresEligibility ==
    [][\A attempt \in assigned' \ assigned: GroupEligible(groups[attempt])]_vars

WinnersStable ==
    [][ /\ \A file \in Completed: winner'[file] = winner[file]
        /\ committed \subseteq committed'
        /\ published \subseteq published']_vars

GroupsStable ==
    [][\A attempt \in Attempts:
           groups[attempt] # {} => groups'[attempt] = groups[attempt]]_vars

\* Expected-failing reachability checks, enabled only by probe configurations.
NoRejectedLeftover == stranded = {}
\* Require a multi-file winner AND singleton recovery of a previously stranded
\* file, so the witness specifically exercises overlapping groups then retry.
NoRecoveredLeftover ==
    ~( /\ \E attempt \in committed: Cardinality(groups[attempt]) > 1
       /\ \E file \in stranded \cap Completed:
              Cardinality(groups[winner[file]]) = 1 )
NoOlderWinner == \A file \in Completed: winner[file] = latest[file]
NoAmbiguousCommit == \A attempt \in committed: phase[attempt] = "won"
=============================================================================
