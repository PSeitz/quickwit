------------------------- MODULE InputReclamation -------------------------
EXTENDS Naturals, FiniteSets

\* Paired protocol slice: one index, fixed ordered work units already flushed
\* and acknowledged. V2 uses one shard; files uses finished singleton input files.
\* Both use the SAME abstract output production and ordered publication steps.
\* No crashes, retries, reassignment, actor-stage topology, or capacity model.
\* Remote effects and caller observations are separate; either may be delayed.
CONSTANTS Protocol, NumUnits
ASSUME Protocol \in {"v2", "files"} /\ NumUnits \in Nat /\ NumUnits > 0
Units == 1..NumUnits

VARIABLES local, remote, registered, read, ready, published, observed, publication,
          sourceNext, sourceBusy, notified, suggestions, emitted, direct, gossip,
          handoff
vars == <<local, remote, registered, read, ready, published, observed, publication,
          sourceNext, sourceBusy, notified, suggestions, emitted, direct, gossip,
          handoff>>

Completed == 1..published

Init ==
    /\ local = Units /\ remote = {} /\ registered = {} /\ read = {} /\ ready = {}
    /\ published = 0 /\ observed = 0 /\ publication = "idle"
    /\ sourceNext = 1 /\ sourceBusy = FALSE /\ notified = 0
    /\ suggestions = {} /\ emitted = 0 /\ direct = {} /\ gossip = 0
    /\ handoff = [unit |-> 1, stage |-> "idle"]

\* Parsing/indexing/finalizing/output upload are one common abstraction. No
\* claim about how many Rust workers or intermediate batches implement it.
ProduceOutput(unit) ==
    /\ unit \in read \ ready
    /\ ready' = ready \cup {unit}
    /\ UNCHANGED <<local, remote, registered, read, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

SendPublication ==
    /\ publication = "idle" /\ observed < NumUnits /\ observed + 1 \in ready
    /\ Protocol = "v2" => notified = observed
    /\ publication' = "sent"
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

CommitPublication ==
    /\ publication = "sent"
    /\ published' = observed + 1 /\ publication' = "committed"
    /\ UNCHANGED <<local, remote, registered, read, ready, observed,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

ObservePublication ==
    /\ publication = "committed"
    /\ observed' = published /\ publication' = "idle"
    /\ UNCHANGED <<local, remote, registered, read, ready, published,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

\* V2: Source owns a payload between reading and forwarding it. During this
\* handler it cannot process SuggestTruncate. Delaying Forward models a handler
\* still in progress, NOT a literal two-batch filling of the real 10-slot mailbox.
ReadWal ==
    /\ ~sourceBusy /\ sourceNext \in local
    /\ sourceBusy' = TRUE
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, notified, suggestions, emitted, direct,
                    gossip, handoff>>

Forward ==
    /\ sourceBusy
    /\ read' = read \cup {sourceNext}
    /\ sourceNext' = sourceNext + 1 /\ sourceBusy' = FALSE
    /\ UNCHANGED <<local, remote, registered, ready, published, observed,
                    publication, notified, suggestions, emitted, direct, gossip, handoff>>

\* Publisher enqueues the suggestion only after observing publication success,
\* before processing the next publication. Enqueue does not await its handling.
SuggestTruncate ==
    /\ notified < observed
    /\ notified' = observed /\ suggestions' = suggestions \cup {observed}
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, emitted, direct, gossip, handoff>>

\* The increasing suggestion positions encode FIFO order without an extra queue
\* variable. Source launches BOTH feedback paths. Neither polls the metastore.
HandleSuggestion ==
    /\ ~sourceBusy /\ suggestions # {}
    /\ LET position == CHOOSE candidate \in suggestions:
                           \A other \in suggestions: candidate <= other
       IN /\ suggestions' = suggestions \ {position}
          /\ emitted' = position /\ direct' = direct \cup {position}
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, notified, gossip, handoff>>

\* Successful logical record removal. RPC acknowledgment has no further effect
\* in this slice and is omitted. Physical WAL segment GC and EOF are excluded.
ApplyDirect(position) ==
    /\ position \in direct
    /\ local' = local \ (1..position) /\ direct' = direct \ {position}
    /\ UNCHANGED <<remote, registered, read, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, gossip, handoff>>

\* Gossip coalesces to the latest emitted position. Delivery and application
\* remain separate, allowing the direct request to overtake either step.
DeliverGossip ==
    /\ gossip < emitted /\ gossip' = emitted
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, notified, suggestions,
                    emitted, direct, handoff>>

ApplyGossip ==
    /\ local \cap (1..gossip) # {}
    /\ local' = local \ (1..gossip)
    /\ UNCHANGED <<remote, registered, read, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

\* Files: one uploader completes upload/register/local-delete before selecting
\* the next finished input, matching the current implementation.
SendUpload ==
    /\ handoff.stage = "idle" /\ handoff.unit \in local
    /\ handoff' = [handoff EXCEPT !.stage = "uploadSent"]
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, notified, suggestions,
                    emitted, direct, gossip>>

CommitUpload ==
    /\ handoff.stage = "uploadSent"
    /\ remote' = remote \cup {handoff.unit}
    /\ handoff' = [handoff EXCEPT !.stage = "uploadCommitted"]
    /\ UNCHANGED <<local, registered, read, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct, gossip>>

ObserveUpload ==
    /\ handoff.stage = "uploadCommitted"
    /\ handoff' = [handoff EXCEPT !.stage = "uploaded"]
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, notified, suggestions,
                    emitted, direct, gossip>>

SendRegistration ==
    /\ handoff.stage = "uploaded"
    /\ handoff' = [handoff EXCEPT !.stage = "registrationSent"]
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, notified, suggestions,
                    emitted, direct, gossip>>

CommitRegistration ==
    /\ handoff.stage = "registrationSent"
    /\ registered' = registered \cup {handoff.unit}
    /\ handoff' = [handoff EXCEPT !.stage = "registrationCommitted"]
    /\ UNCHANGED <<local, remote, read, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct, gossip>>

ObserveRegistration ==
    /\ handoff.stage = "registrationCommitted"
    /\ handoff' = [handoff EXCEPT !.stage = "registered"]
    /\ UNCHANGED <<local, remote, registered, read, ready, published, observed,
                    publication, sourceNext, sourceBusy, notified, suggestions,
                    emitted, direct, gossip>>

DeleteLocal ==
    /\ handoff.stage = "registered"
    /\ local' = local \ {handoff.unit}
    /\ handoff' = [unit |-> handoff.unit + 1, stage |-> "idle"]
    /\ UNCHANGED <<remote, registered, read, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct, gossip>>

ReadRemote(unit) ==
    /\ unit \in (registered \cap remote) \ read
    /\ read' = read \cup {unit}
    /\ UNCHANGED <<local, remote, registered, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

\* Proposed completed-input collector reads durable completion, independently
\* of the publication caller's response. Completed evidence is retained.
DeleteRemote(unit) ==
    /\ unit \in remote \cap Completed
    /\ remote' = remote \ {unit}
    /\ UNCHANGED <<local, registered, read, ready, published, observed, publication,
                    sourceNext, sourceBusy, notified, suggestions, emitted, direct,
                    gossip, handoff>>

Next ==
    \/ \E unit \in Units: ProduceOutput(unit)
    \/ SendPublication \/ CommitPublication \/ ObservePublication
    \/ IF Protocol = "v2"
       THEN \/ ReadWal \/ Forward \/ SuggestTruncate \/ HandleSuggestion
            \/ \E position \in Units: ApplyDirect(position)
            \/ DeliverGossip \/ ApplyGossip
       ELSE \/ SendUpload \/ CommitUpload \/ ObserveUpload
            \/ SendRegistration \/ CommitRegistration \/ ObserveRegistration \/ DeleteLocal
            \/ \E unit \in Units: ReadRemote(unit) \/ DeleteRemote(unit)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ \A units \in {local, remote, registered, read, ready, suggestions, direct}:
           units \subseteq Units
    /\ \A position \in {published, observed, notified, emitted, gossip}: position \in 0..NumUnits
    /\ sourceNext \in 1..(NumUnits + 1) /\ sourceBusy \in BOOLEAN
    /\ publication \in {"idle", "sent", "committed"}
    /\ handoff \in [unit : 1..(NumUnits + 1),
                   stage : {"idle", "uploadSent", "uploadCommitted", "uploaded",
                            "registrationSent", "registrationCommitted", "registered"}]

AcknowledgedRecoverable == Units \subseteq local \cup remote \cup Completed
PublishedOutputsExist == Completed \subseteq ready /\ ready \subseteq read
LocalDeletionAuthorized ==
    Units \ local \subseteq IF Protocol = "v2" THEN Completed ELSE registered
RegisteredInputAvailable == registered \ Completed \subseteq remote
PublicationOrder == /\ observed <= published /\ published <= observed + 1
                    /\ (publication = "committed") <=> (observed < published)
FeedbackAuthority ==
    /\ gossip <= emitted /\ emitted <= notified /\ notified <= observed
    /\ suggestions \subseteq 1..notified /\ direct \subseteq 1..emitted

LocalDeletionRequiresConfirmation ==
    [][(Protocol = "files" /\ local' # local) => handoff.stage = "registered"]_vars
CompletionStable == [][published' >= published]_vars

\* Negated witness predicates, used only by the runner's reachability checks.
NoLocalReclamationBeforePublication == Units \ local \subseteq Completed
NoFeedbackBehindSource == ~(sourceBusy /\ suggestions # {} /\ local \cap Completed # {})
NoFullyReclaimed == ~(published = NumUnits /\ local = {} /\ remote = {})
NoPublicationBeforeRegistrationReply ==
    ~(handoff.stage = "registrationCommitted" /\ handoff.unit <= published)
NoRemoteDeletionBeforePublicationReply ==
    ~(\E unit \in registered \ remote: observed < unit /\ unit <= published)
=============================================================================
