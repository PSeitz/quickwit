-------------------------- MODULE IngestLifecycle --------------------------
EXTENDS FiniteSets

\* With an uploader configured, startup uploads, registers, and deletes recovered
\* local files before intake; drain hands off every accepted write before success.
\* Each identity is one valid append/file. Append/flush and recovery IO are atomic
\* here; byte framing and flush internals belong to IngestHandoff and Rust tests.
\* uploaded/knownRegistered represent observed per-file stages, not Rust ID sets.
\* Cross-file handoff scheduling is abstract; each file preserves stage ordering.
CONSTANTS Files, Recovered
ASSUME /\ IsFiniteSet(Files) /\ Files # {}
       /\ Recovered \subseteq Files /\ "none" \notin Files

Modes == {"startup", "accepting", "finishing", "draining", "stopping", "stopped"}
VARIABLES mode, owned, queued, used, flushed, acknowledged, local, finished,
          charged, recovery, uploaded, knownRegistered, objects, registered,
          uploads, registrations, deleting, removed, drainTarget, drained
vars == <<mode, owned, queued, used, flushed, acknowledged, local, finished,
          charged, recovery, uploaded, knownRegistered, objects, registered,
          uploads, registrations, deleting, removed, drainTarget, drained>>

Init ==
    /\ mode = "startup" /\ owned = TRUE
    /\ queued = {} /\ used = Recovered /\ flushed = Recovered
    /\ acknowledged = Recovered /\ local = Recovered /\ finished = Recovered
    /\ charged = Recovered /\ recovery = Recovered
    /\ uploaded = {} /\ knownRegistered = {}
    /\ objects = {} /\ registered = {} /\ uploads = {} /\ registrations = {}
    /\ deleting = "none" /\ removed = FALSE
    /\ drainTarget = {} /\ drained = FALSE

OpenIntake ==
    /\ mode = "startup" /\ local = {} /\ charged = {} /\ deleting = "none"
    /\ mode' = "accepting"
    /\ UNCHANGED <<owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

Admit(file) ==
    /\ mode = "accepting" /\ file \notin used
    /\ queued' = queued \cup {file} /\ used' = used \cup {file}
    /\ UNCHANGED <<mode, owned, flushed, acknowledged, local, finished, charged,
                    recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

\* An admitted request survives caller cancellation. A process failure may lose
\* queued, unacknowledged work. A successful append includes the kernel flush.
Write(file) ==
    /\ mode \in {"accepting", "finishing", "draining"} /\ file \in queued
    /\ queued' = queued \ {file} /\ flushed' = flushed \cup {file}
    /\ local' = local \cup {file} /\ charged' = charged \cup {file}
    /\ UNCHANGED <<mode, owned, used, acknowledged, finished, recovery, uploaded,
                    knownRegistered, objects, registered, uploads, registrations,
                    deleting, removed, drainTarget, drained>>

Acknowledge(file) ==
    /\ file \in flushed \ acknowledged
    /\ acknowledged' = acknowledged \cup {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, local, finished, charged,
                    recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

FinishFile(file) ==
    /\ mode \in {"accepting", "finishing", "draining"}
    /\ file \in local \ finished
    /\ finished' = finished \cup {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, charged,
                    recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

SendUpload(file) ==
    /\ mode \in {"startup", "accepting", "finishing", "draining"}
    /\ file \in finished \ uploaded /\ file \notin uploads
    /\ uploads' = uploads \cup {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    registrations, deleting, removed, drainTarget, drained>>

\* Remote effects can finish after cancellation or process death. Lost responses
\* are represented by withholding observation; retries use the same immutable key.
CommitUpload(file) ==
    /\ file \in uploads
    /\ objects' = objects \cup {file} /\ uploads' = uploads \ {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, registered,
                    registrations, deleting, removed, drainTarget, drained>>

ObserveUpload(file) ==
    /\ mode \in {"startup", "accepting", "finishing", "draining"}
    /\ file \in finished \cap objects /\ file \notin uploaded
    /\ uploaded' = uploaded \cup {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

SendRegistration(file) ==
    /\ mode \in {"startup", "accepting", "finishing", "draining"}
    /\ file \in finished \cap uploaded /\ file \notin knownRegistered
    /\ file \notin registrations
    /\ registrations' = registrations \cup {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, deleting, removed, drainTarget, drained>>

CommitRegistration(file) ==
    /\ file \in registrations
    /\ registered' = registered \cup {file}
    /\ registrations' = registrations \ {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects,
                    uploads, deleting, removed, drainTarget, drained>>

ObserveRegistration(file) ==
    /\ mode \in {"startup", "accepting", "finishing", "draining"}
    /\ file \in finished \cap registered /\ file \notin knownRegistered
    /\ knownRegistered' = knownRegistered \cup {file}
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

StartDeletion(file) ==
    /\ mode \in {"startup", "accepting", "finishing", "draining"} /\ deleting = "none"
    /\ file \in finished \cap knownRegistered
    /\ deleting' = file /\ removed' = FALSE
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, drainTarget, drained>>

\* Blocking filesystem work survives async cancellation. Ownership must outlive
\* that work. A process crash ends it; recovery recounts surviving local files.
RemoveLocal ==
    /\ deleting # "none" /\ ~removed
    /\ local' = local \ {deleting} /\ finished' = finished \ {deleting}
    /\ removed' = TRUE
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, charged,
                    recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, drainTarget, drained>>

FinishDeletion ==
    /\ deleting # "none" /\ removed
    /\ charged' = charged \ {deleting}
    /\ deleting' = "none" /\ removed' = FALSE
    /\ UNCHANGED <<mode, owned, queued, used, flushed, acknowledged, local, finished,
                    recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, drainTarget, drained>>

\* Closing admission precedes processing the FIFO finish/drain marker. Handoff
\* may continue while queued writes finish. Queue ordering itself is abstract.
CloseIntake(nextMode) ==
    /\ mode = "accepting" /\ nextMode \in {"finishing", "draining"}
    /\ mode' = nextMode
    /\ drainTarget' = IF nextMode = "draining" THEN flushed \cup queued ELSE {}
    /\ UNCHANGED <<owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drained>>

FinishLocally ==
    /\ mode = "finishing" /\ queued = {} /\ local = finished
    /\ mode' = "stopping"
    /\ UNCHANGED <<owned, queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

DrainSuccess ==
    /\ mode = "draining" /\ queued = {} /\ local = {} /\ charged = {}
    /\ deleting = "none"
    /\ mode' = "stopped" /\ owned' = FALSE /\ drained' = TRUE
    /\ UNCHANGED <<queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget>>

\* Startup/drain cancellation or a permanent worker error stops progress without
\* claiming drain success. Unflushed queued requests can fail. Transient failures
\* and retry delays are stuttering, with no availability or fairness assumption.
Stop ==
    /\ mode \in {"startup", "accepting", "finishing", "draining"}
    /\ mode' = "stopping" /\ queued' = {}
    /\ UNCHANGED <<owned, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

ReleaseDirectory ==
    /\ mode = "stopping" /\ deleting = "none"
    /\ mode' = "stopped" /\ owned' = FALSE
    /\ UNCHANGED <<queued, used, flushed, acknowledged, local, finished,
                    charged, recovery, uploaded, knownRegistered, objects, registered,
                    uploads, registrations, deleting, removed, drainTarget, drained>>

Crash ==
    /\ owned
    /\ mode' = "stopped" /\ owned' = FALSE /\ queued' = {}
    /\ charged' = local /\ deleting' = "none" /\ removed' = FALSE
    /\ uploaded' = {} /\ knownRegistered' = {}
    /\ UNCHANGED <<used, flushed, acknowledged, local, finished, recovery,
                    objects, registered, uploads, registrations, drainTarget, drained>>

Recover ==
    /\ mode = "stopped"
    /\ mode' = "startup" /\ owned' = TRUE /\ finished' = local
    /\ recovery' = local /\ uploaded' = {} /\ knownRegistered' = {}
    /\ drainTarget' = {} /\ drained' = FALSE
    /\ UNCHANGED <<queued, used, flushed, acknowledged, local, charged,
                    objects, registered, uploads, registrations, deleting, removed>>

Next ==
    \/ OpenIntake \/ FinishLocally \/ DrainSuccess \/ Stop \/ ReleaseDirectory
    \/ Crash \/ Recover \/ RemoveLocal \/ FinishDeletion
    \/ CloseIntake("finishing") \/ CloseIntake("draining")
    \/ \E file \in Files:
           \/ Admit(file) \/ Write(file) \/ Acknowledge(file) \/ FinishFile(file)
           \/ SendUpload(file) \/ CommitUpload(file) \/ ObserveUpload(file)
           \/ SendRegistration(file) \/ CommitRegistration(file)
           \/ ObserveRegistration(file) \/ StartDeletion(file)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ mode \in Modes /\ owned \in BOOLEAN /\ removed \in BOOLEAN /\ drained \in BOOLEAN
    /\ deleting \in Files \cup {"none"}
    /\ \A files \in {queued, used, flushed, acknowledged, local, finished, charged,
                     recovery, uploaded, knownRegistered, objects, registered,
                     uploads, registrations, drainTarget}: files \subseteq Files

AckAfterFlush == acknowledged \subseteq flushed
AcknowledgedRecoverable == acknowledged \subseteq local \cup objects
HandoffBeforeLocalDeletion == flushed \ local \subseteq registered
RegistrationAfterUpload == registered \subseteq objects
LocalAccounting == /\ local \subseteq charged /\ finished \subseteq local
DirectoryOwnership == /\ owned = (mode # "stopped")
                      /\ deleting # "none" => owned
RecoveryHandedOff == /\ recovery \subseteq registered
                    /\ recovery \cap (local \cup charged) = {}
                    /\ deleting \notin recovery
StartupGate == mode \in {"accepting", "finishing", "draining"} => RecoveryHandedOff
DrainComplete == drained =>
    /\ mode = "stopped" /\ ~owned /\ queued = {} /\ local = {} /\ charged = {}
    /\ drainTarget \subseteq registered /\ drainTarget \subseteq objects

AdmissionRequiresIntake == [][(queued' \ queued # {}) =>
    (mode = "accepting" /\ RecoveryHandedOff)]_vars
RegistrationRequiresUpload == [][(registrations' \ registrations) \subseteq uploaded]_vars

\* Expected-failing invariants used only by the reachability configurations.
\* They establish possible executions, rather than eventual progress.
NoIntakeAfterRecovery == ~(mode = "accepting" /\ recovery # {} /\ queued # {})
NoStartupCancellationDuringDeletion ==
    ~(mode = "stopping" /\ deleting \in recovery)
NoDrainWithNewInput == ~(drained /\ drainTarget \ Recovered # {})
NoDrainCancellationDuringDeletion ==
    ~(mode = "stopping" /\ drainTarget # {} /\ deleting # "none")
=============================================================================
