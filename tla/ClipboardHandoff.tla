---- MODULE ClipboardHandoff ----
EXTENDS Naturals

\* Finite role abstraction. SERVER_HOST is the Lan Mouse ownership authority;
\* it is not an operating-system identity.
Host == {"server", "remote"}
SERVER_HOST == "server"
REMOTE_HOST == "remote"

Contents == {"empty", "alpha", "beta"}
SnapshotKinds == {"none", "text", "empty"}
SwitchPhases == {"idle", "pending"}
HandoffPhases == {
    "none",
    "capturing",
    "ready",
    "in_flight",
    "staged",
    "applied",
    "skipped",
    "canceled"
}
ActiveHandoffPhases == {"capturing", "ready", "in_flight", "staged"}

CONSTANTS MAX_EPOCH, MAX_GENERATION, MAX_RETRIES, MAX_PAYLOAD, MAX_SESSION

ASSUME /\ MAX_EPOCH \in Nat /\ MAX_EPOCH >= 2
       /\ MAX_GENERATION \in Nat /\ MAX_GENERATION > 0
       /\ MAX_RETRIES \in Nat /\ MAX_RETRIES > 0
       /\ MAX_PAYLOAD \in Nat /\ MAX_PAYLOAD > 0
       /\ MAX_SESSION \in Nat

Session == 0..MAX_SESSION
Epoch == 0..MAX_EPOCH
Generation == 0..MAX_GENERATION
PayloadSize == 0..(MAX_PAYLOAD + 1)

HandoffType == [
    phase                   : HandoffPhases,
    source                  : Host,
    target                  : Host,
    source_authority        : Session,
    source_epoch            : Epoch,
    source_process          : Session,
    target_authority        : Session,
    target_epoch            : Epoch,
    target_process          : Session,
    handoff_epoch           : Epoch,
    source_start_generation : Generation,
    retries                 : 0..MAX_RETRIES,
    snapshot_kind           : SnapshotKinds,
    snapshot_value          : Contents,
    snapshot_size           : PayloadSize,
    snapshot_private        : BOOLEAN,
    target_prepared         : BOOLEAN,
    target_baseline         : Generation,
    target_activated        : BOOLEAN
]

WireType == [
    pending          : BOOLEAN,
    source           : Host,
    target           : Host,
    source_authority : Session,
    source_epoch     : Epoch,
    source_process   : Session,
    target_authority : Session,
    target_epoch     : Epoch,
    target_process   : Session,
    handoff_epoch    : Epoch,
    snapshot_kind    : SnapshotKinds,
    snapshot_value   : Contents,
    snapshot_size    : PayloadSize,
    integrity        : BOOLEAN
]

StageType == [
    valid            : BOOLEAN,
    source           : Host,
    target           : Host,
    source_authority : Session,
    source_epoch     : Epoch,
    source_process   : Session,
    target_authority : Session,
    target_epoch     : Epoch,
    target_process   : Session,
    handoff_epoch    : Epoch,
    snapshot_kind    : SnapshotKinds,
    snapshot_value   : Contents,
    snapshot_size    : PayloadSize,
    target_baseline  : Generation
]

RetiredType == [
    valid            : BOOLEAN,
    source           : Host,
    target           : Host,
    source_authority : Session,
    source_epoch     : Epoch,
    source_process   : Session,
    target_authority : Session,
    target_epoch     : Epoch,
    target_process   : Session,
    handoff_epoch    : Epoch,
    snapshot_kind    : SnapshotKinds,
    snapshot_value   : Contents,
    snapshot_size    : PayloadSize
]

VARIABLES
    authority_session,
    process_session,
    owner_epoch,
    next_epoch,
    next_handoff_epoch,
    input_owner,
    keyboard_owner,
    pointer_owner,
    input_ready,
    switch_phase,
    switch_source,
    switch_target,
    switch_epoch,
    switch_handoff_epoch,
    clipboard_enabled,
    channel_up,
    backend_ready,
    native_generation,
    native_value,
    native_private,
    native_size,
    handoff,
    wire,
    stage,
    retired,
    applied_valid,
    applied_session,
    applied_handoff_epoch

inputVars == <<
    authority_session,
    owner_epoch,
    next_epoch,
    next_handoff_epoch,
    input_owner,
    keyboard_owner,
    pointer_owner,
    input_ready,
    switch_phase,
    switch_source,
    switch_target,
    switch_epoch,
    switch_handoff_epoch
>>

nativeVars == <<native_generation, native_value, native_private, native_size>>

clipboardVars == <<
    process_session,
    clipboard_enabled,
    channel_up,
    backend_ready,
    handoff,
    wire,
    stage,
    retired,
    applied_valid,
    applied_session,
    applied_handoff_epoch
>>

vars == <<inputVars, nativeVars, clipboardVars>>

EmptyHandoff == [
    phase                   |-> "none",
    source                  |-> SERVER_HOST,
    target                  |-> REMOTE_HOST,
    source_authority        |-> 0,
    source_epoch            |-> 0,
    source_process          |-> 0,
    target_authority        |-> 0,
    target_epoch            |-> 0,
    target_process          |-> 0,
    handoff_epoch           |-> 0,
    source_start_generation |-> 0,
    retries                 |-> 0,
    snapshot_kind           |-> "none",
    snapshot_value          |-> "empty",
    snapshot_size           |-> 0,
    snapshot_private        |-> FALSE,
    target_prepared         |-> FALSE,
    target_baseline         |-> 0,
    target_activated        |-> FALSE
]

EmptyWire == [
    pending          |-> FALSE,
    source           |-> SERVER_HOST,
    target           |-> REMOTE_HOST,
    source_authority |-> 0,
    source_epoch     |-> 0,
    source_process   |-> 0,
    target_authority |-> 0,
    target_epoch     |-> 0,
    target_process   |-> 0,
    handoff_epoch    |-> 0,
    snapshot_kind    |-> "none",
    snapshot_value   |-> "empty",
    snapshot_size    |-> 0,
    integrity        |-> TRUE
]

EmptyStage == [
    valid            |-> FALSE,
    source           |-> SERVER_HOST,
    target           |-> REMOTE_HOST,
    source_authority |-> 0,
    source_epoch     |-> 0,
    source_process   |-> 0,
    target_authority |-> 0,
    target_epoch     |-> 0,
    target_process   |-> 0,
    handoff_epoch    |-> 0,
    snapshot_kind    |-> "none",
    snapshot_value   |-> "empty",
    snapshot_size    |-> 0,
    target_baseline  |-> 0
]

EmptyRetired == [
    valid            |-> FALSE,
    source           |-> SERVER_HOST,
    target           |-> REMOTE_HOST,
    source_authority |-> 0,
    source_epoch     |-> 0,
    source_process   |-> 0,
    target_authority |-> 0,
    target_epoch     |-> 0,
    target_process   |-> 0,
    handoff_epoch    |-> 0,
    snapshot_kind    |-> "none",
    snapshot_value   |-> "empty",
    snapshot_size    |-> 0
]

NewHandoff(source, target, targetEpoch, handoffEpoch) == [
    phase                   |-> IF clipboard_enabled THEN "capturing" ELSE "skipped",
    source                  |-> source,
    target                  |-> target,
    source_authority        |-> authority_session,
    source_epoch            |-> owner_epoch,
    source_process          |-> process_session[source],
    target_authority        |-> authority_session,
    target_epoch            |-> targetEpoch,
    target_process          |-> process_session[target],
    handoff_epoch           |-> handoffEpoch,
    source_start_generation |-> native_generation[source],
    retries                 |-> 0,
    snapshot_kind           |-> "none",
    snapshot_value          |-> "empty",
    snapshot_size           |-> 0,
    snapshot_private        |-> FALSE,
    target_prepared         |-> FALSE,
    target_baseline         |-> native_generation[target],
    target_activated        |-> FALSE
]

RetireHandoff(h) == [
    valid            |-> h.phase # "none",
    source           |-> h.source,
    target           |-> h.target,
    source_authority |-> h.source_authority,
    source_epoch     |-> h.source_epoch,
    source_process   |-> h.source_process,
    target_authority |-> h.target_authority,
    target_epoch     |-> h.target_epoch,
    target_process   |-> h.target_process,
    handoff_epoch    |-> h.handoff_epoch,
    snapshot_kind    |-> h.snapshot_kind,
    snapshot_value   |-> h.snapshot_value,
    snapshot_size    |-> h.snapshot_size
]

WireFromHandoff(h) == [
    pending          |-> TRUE,
    source           |-> h.source,
    target           |-> h.target,
    source_authority |-> h.source_authority,
    source_epoch     |-> h.source_epoch,
    source_process   |-> h.source_process,
    target_authority |-> h.target_authority,
    target_epoch     |-> h.target_epoch,
    target_process   |-> h.target_process,
    handoff_epoch    |-> h.handoff_epoch,
    snapshot_kind    |-> h.snapshot_kind,
    snapshot_value   |-> h.snapshot_value,
    snapshot_size    |-> h.snapshot_size,
    integrity        |-> TRUE
]

WireFromRetired(r) == [
    pending          |-> TRUE,
    source           |-> r.source,
    target           |-> r.target,
    source_authority |-> r.source_authority,
    source_epoch     |-> r.source_epoch,
    source_process   |-> r.source_process,
    target_authority |-> r.target_authority,
    target_epoch     |-> r.target_epoch,
    target_process   |-> r.target_process,
    handoff_epoch    |-> r.handoff_epoch,
    snapshot_kind    |-> r.snapshot_kind,
    snapshot_value   |-> r.snapshot_value,
    snapshot_size    |-> r.snapshot_size,
    integrity        |-> TRUE
]

StageFromWire(w, baseline) == [
    valid            |-> TRUE,
    source           |-> w.source,
    target           |-> w.target,
    source_authority |-> w.source_authority,
    source_epoch     |-> w.source_epoch,
    source_process   |-> w.source_process,
    target_authority |-> w.target_authority,
    target_epoch     |-> w.target_epoch,
    target_process   |-> w.target_process,
    handoff_epoch    |-> w.handoff_epoch,
    snapshot_kind    |-> w.snapshot_kind,
    snapshot_value   |-> w.snapshot_value,
    snapshot_size    |-> w.snapshot_size,
    target_baseline  |-> baseline
]

WellFormedContent(value, size) ==
    IF value = "empty" THEN size = 0 ELSE size \in 1..(MAX_PAYLOAD + 1)

BumpGeneration(g) == IF g < MAX_GENERATION THEN g + 1 ELSE g

ActiveHandoff == handoff.phase \in ActiveHandoffPhases

PendingMatchesHandoff ==
    /\ switch_phase = "pending"
    /\ switch_source = handoff.source
    /\ switch_target = handoff.target
    /\ switch_epoch = handoff.target_epoch
    /\ switch_handoff_epoch = handoff.handoff_epoch
    /\ handoff.source_authority = authority_session
    /\ handoff.source_epoch = owner_epoch
    /\ handoff.target_authority = authority_session
    /\ input_owner = handoff.source

TargetIsCurrent ==
    /\ input_owner = handoff.target
    /\ handoff.target_authority = authority_session
    /\ handoff.target_epoch = owner_epoch

HandoffProcessesFresh ==
    /\ process_session[handoff.source] = handoff.source_process
    /\ process_session[handoff.target] = handoff.target_process

HandoffRelevant ==
    /\ ActiveHandoff
    /\ HandoffProcessesFresh
    /\ (PendingMatchesHandoff \/ TargetIsCurrent)

SourceStillCurrent ==
    /\ input_owner = handoff.source
    /\ authority_session = handoff.source_authority
    /\ owner_epoch = handoff.source_epoch
    /\ process_session[handoff.source] = handoff.source_process

WireMatchesHandoff ==
    /\ wire.source = handoff.source
    /\ wire.target = handoff.target
    /\ wire.source_authority = handoff.source_authority
    /\ wire.source_epoch = handoff.source_epoch
    /\ wire.source_process = handoff.source_process
    /\ wire.target_authority = handoff.target_authority
    /\ wire.target_epoch = handoff.target_epoch
    /\ wire.target_process = handoff.target_process
    /\ wire.handoff_epoch = handoff.handoff_epoch

WireMatchesActiveHandoff ==
    /\ handoff.phase = "in_flight"
    /\ WireMatchesHandoff

StageMatchesHandoff ==
    /\ stage.source = handoff.source
    /\ stage.target = handoff.target
    /\ stage.source_authority = handoff.source_authority
    /\ stage.source_epoch = handoff.source_epoch
    /\ stage.source_process = handoff.source_process
    /\ stage.target_authority = handoff.target_authority
    /\ stage.target_epoch = handoff.target_epoch
    /\ stage.target_process = handoff.target_process
    /\ stage.handoff_epoch = handoff.handoff_epoch

AlreadyApplied(target, session, handoffEpoch) ==
    /\ applied_valid[target]
    /\ applied_session[target] = session
    /\ applied_handoff_epoch[target] = handoffEpoch

SnapshotKindMatches(kind, value, size) ==
    \/ /\ kind = "empty" /\ value = "empty" /\ size = 0
    \/ /\ kind = "text" /\ value # "empty" /\ size \in 1..MAX_PAYLOAD

Init ==
    /\ authority_session = 0
    /\ process_session = [h \in Host |-> 0]
    /\ owner_epoch = 0
    /\ next_epoch = 0
    /\ next_handoff_epoch = 0
    /\ input_owner = SERVER_HOST
    /\ keyboard_owner = SERVER_HOST
    /\ pointer_owner = SERVER_HOST
    /\ input_ready = [h \in Host |-> TRUE]
    /\ switch_phase = "idle"
    /\ switch_source = SERVER_HOST
    /\ switch_target = REMOTE_HOST
    /\ switch_epoch = 0
    /\ switch_handoff_epoch = 0
    /\ clipboard_enabled \in BOOLEAN
    /\ channel_up \in BOOLEAN
    /\ backend_ready = [h \in Host |-> TRUE]
    /\ native_generation = [h \in Host |-> 0]
    /\ native_value = [h \in Host |-> IF h = SERVER_HOST THEN "alpha" ELSE "beta"]
    /\ native_private = [h \in Host |-> FALSE]
    /\ native_size = [h \in Host |-> 1]
    /\ handoff = EmptyHandoff
    /\ wire = EmptyWire
    /\ stage = EmptyStage
    /\ retired = EmptyRetired
    /\ applied_valid = [h \in Host |-> FALSE]
    /\ applied_session = [h \in Host |-> 0]
    /\ applied_handoff_epoch = [h \in Host |-> 0]

TypeInvariant ==
    /\ authority_session \in Session
    /\ process_session \in [Host -> Session]
    /\ owner_epoch \in Epoch
    /\ next_epoch \in Epoch
    /\ next_handoff_epoch \in Epoch
    /\ input_owner \in Host
    /\ keyboard_owner \in Host
    /\ pointer_owner \in Host
    /\ input_ready \in [Host -> BOOLEAN]
    /\ switch_phase \in SwitchPhases
    /\ switch_source \in Host
    /\ switch_target \in Host
    /\ switch_epoch \in Epoch
    /\ switch_handoff_epoch \in Epoch
    /\ clipboard_enabled \in BOOLEAN
    /\ channel_up \in BOOLEAN
    /\ backend_ready \in [Host -> BOOLEAN]
    /\ native_generation \in [Host -> Generation]
    /\ native_value \in [Host -> Contents]
    /\ native_private \in [Host -> BOOLEAN]
    /\ native_size \in [Host -> PayloadSize]
    /\ \A h \in Host : WellFormedContent(native_value[h], native_size[h])
    /\ handoff \in HandoffType
    /\ wire \in WireType
    /\ stage \in StageType
    /\ retired \in RetiredType
    /\ applied_valid \in [Host -> BOOLEAN]
    /\ applied_session \in [Host -> Session]
    /\ applied_handoff_epoch \in [Host -> Epoch]

InputOwnershipAtomic ==
    /\ keyboard_owner = pointer_owner
    /\ keyboard_owner = input_owner

ServerProcessTracksAuthority ==
    process_session[SERVER_HOST] = authority_session

MonotonicIdentityAllocation ==
    /\ owner_epoch <= next_epoch
    /\ next_epoch = next_handoff_epoch
    /\ switch_phase = "pending" =>
        /\ switch_epoch = next_epoch
        /\ switch_handoff_epoch = next_handoff_epoch

PendingSwitchWellFormed ==
    switch_phase = "pending" =>
        /\ switch_source = input_owner
        /\ switch_target # input_owner
        /\ switch_epoch > owner_epoch

ActiveHandoffFenced ==
    ActiveHandoff =>
        /\ handoff.source # handoff.target
        /\ handoff.source_authority = authority_session
        /\ handoff.target_authority = authority_session
        /\ (PendingMatchesHandoff \/ TargetIsCurrent)

PreparedBeforeActivated ==
    handoff.target_activated => handoff.target_prepared

ClipboardDisabledSettled ==
    ~clipboard_enabled =>
        /\ ~ActiveHandoff
        /\ ~wire.pending
        /\ ~stage.valid

PayloadBounded ==
    /\ handoff.phase \in {"ready", "in_flight", "staged", "applied"} =>
        /\ handoff.snapshot_size <= MAX_PAYLOAD
        /\ SnapshotKindMatches(
              handoff.snapshot_kind,
              handoff.snapshot_value,
              handoff.snapshot_size)
    /\ wire.pending /\ wire.snapshot_kind # "none" =>
        /\ wire.snapshot_size <= MAX_PAYLOAD
        /\ SnapshotKindMatches(wire.snapshot_kind, wire.snapshot_value, wire.snapshot_size)
    /\ stage.valid =>
        /\ stage.snapshot_size <= MAX_PAYLOAD
        /\ SnapshotKindMatches(stage.snapshot_kind, stage.snapshot_value, stage.snapshot_size)

NoPrivatePayload ==
    handoff.phase \in {"ready", "in_flight", "staged", "applied"} =>
        ~handoff.snapshot_private

StageIdentityBound ==
    stage.valid =>
        /\ handoff.phase = "staged"
        /\ StageMatchesHandoff
        /\ handoff.target_prepared
        /\ stage.target_baseline = handoff.target_baseline

AtMostOnceStaging ==
    stage.valid =>
        ~AlreadyApplied(stage.target, stage.target_authority, stage.handoff_epoch)

AppliedIdentityRecorded ==
    handoff.phase = "applied" =>
        /\ applied_valid[handoff.target]
        /\ applied_session[handoff.target] = handoff.target_authority
        /\ applied_handoff_epoch[handoff.target] = handoff.handoff_epoch

BeginSwitch(target) ==
    /\ switch_phase = "idle"
    /\ target \in Host
    /\ target # input_owner
    /\ input_ready[target]
    /\ next_epoch < MAX_EPOCH
    /\ next_handoff_epoch < MAX_EPOCH
    \* A remote commit reserves one final finite-model epoch for fallback.
    /\ target = REMOTE_HOST => next_epoch + 1 < MAX_EPOCH
    /\ switch_phase' = "pending"
    /\ switch_source' = input_owner
    /\ switch_target' = target
    /\ switch_epoch' = next_epoch + 1
    /\ switch_handoff_epoch' = next_handoff_epoch + 1
    /\ next_epoch' = next_epoch + 1
    /\ next_handoff_epoch' = next_handoff_epoch + 1
    /\ handoff' = NewHandoff(
         input_owner,
         target,
         next_epoch + 1,
         next_handoff_epoch + 1)
    /\ retired' = IF handoff.phase # "none" THEN RetireHandoff(handoff) ELSE retired
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<
         authority_session,
         process_session,
         owner_epoch,
         input_owner,
         keyboard_owner,
         pointer_owner,
         input_ready,
         clipboard_enabled,
         channel_up,
         backend_ready,
         native_generation,
         native_value,
         native_private,
         native_size,
         applied_valid,
         applied_session,
         applied_handoff_epoch
       >>

BeginRemote == BeginSwitch(REMOTE_HOST)
BeginFallback == BeginSwitch(SERVER_HOST)

CommitSwitch ==
    /\ switch_phase = "pending"
    /\ input_ready[switch_target]
    /\ input_owner' = switch_target
    /\ keyboard_owner' = switch_target
    /\ pointer_owner' = switch_target
    /\ owner_epoch' = switch_epoch
    /\ switch_phase' = "idle"
    /\ UNCHANGED <<
         authority_session,
         process_session,
         next_epoch,
         next_handoff_epoch,
         input_ready,
         switch_source,
         switch_target,
         switch_epoch,
         switch_handoff_epoch,
         clipboard_enabled,
         channel_up,
         backend_ready,
         native_generation,
         native_value,
         native_private,
         native_size,
         handoff,
         wire,
         stage,
         retired,
         applied_valid,
         applied_session,
         applied_handoff_epoch
       >>

AbortSwitch ==
    /\ switch_phase = "pending"
    /\ switch_phase' = "idle"
    /\ handoff' = [handoff EXCEPT !.phase = "canceled"]
    /\ retired' = RetireHandoff(handoff)
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<
         authority_session,
         process_session,
         owner_epoch,
         next_epoch,
         next_handoff_epoch,
         input_owner,
         keyboard_owner,
         pointer_owner,
         input_ready,
         switch_source,
         switch_target,
         switch_epoch,
         switch_handoff_epoch,
         clipboard_enabled,
         channel_up,
         backend_ready,
         native_generation,
         native_value,
         native_private,
         native_size,
         applied_valid,
         applied_session,
         applied_handoff_epoch
       >>

SetClipboardEnabled(enabled) ==
    /\ enabled \in BOOLEAN
    /\ clipboard_enabled # enabled
    /\ clipboard_enabled' = enabled
    /\ IF enabled
          THEN
              /\ UNCHANGED <<handoff, wire, stage, retired>>
          ELSE
              /\ handoff' = IF ActiveHandoff
                                THEN [handoff EXCEPT !.phase = "canceled"]
                                ELSE handoff
              /\ wire' = EmptyWire
              /\ stage' = EmptyStage
              /\ retired' = IF ActiveHandoff
                                THEN RetireHandoff(handoff)
                                ELSE retired
    /\ UNCHANGED <<
         inputVars,
         process_session,
         channel_up,
         backend_ready,
         nativeVars,
         applied_valid,
         applied_session,
         applied_handoff_epoch
       >>

SetInputReady(host, ready) ==
    /\ host \in Host
    /\ ready \in BOOLEAN
    /\ input_ready[host] # ready
    /\ input_ready' = [input_ready EXCEPT ![host] = ready]
    /\ UNCHANGED <<
         authority_session,
         process_session,
         owner_epoch,
         next_epoch,
         next_handoff_epoch,
         input_owner,
         keyboard_owner,
         pointer_owner,
         switch_phase,
         switch_source,
         switch_target,
         switch_epoch,
         switch_handoff_epoch,
         clipboard_enabled,
         channel_up,
         backend_ready,
         native_generation,
         native_value,
         native_private,
         native_size,
         handoff,
         wire,
         stage,
         retired,
         applied_valid,
         applied_session,
         applied_handoff_epoch
       >>

RestartAuthority ==
    /\ authority_session < MAX_SESSION
    /\ process_session[SERVER_HOST] < MAX_SESSION
    /\ authority_session' = authority_session + 1
    /\ process_session' = [process_session EXCEPT ![SERVER_HOST] = @ + 1]
    /\ owner_epoch' = 0
    /\ next_epoch' = 0
    /\ next_handoff_epoch' = 0
    /\ input_owner' = SERVER_HOST
    /\ keyboard_owner' = SERVER_HOST
    /\ pointer_owner' = SERVER_HOST
    /\ switch_phase' = "idle"
    /\ switch_source' = SERVER_HOST
    /\ switch_target' = REMOTE_HOST
    /\ switch_epoch' = 0
    /\ switch_handoff_epoch' = 0
    /\ handoff' = EmptyHandoff
    \* Keep an old wire frame so TLC can try to deliver it in the new session.
    /\ wire' = wire
    /\ stage' = EmptyStage
    /\ retired' = IF handoff.phase # "none" THEN RetireHandoff(handoff) ELSE retired
    /\ UNCHANGED <<
         input_ready,
         clipboard_enabled,
         channel_up,
         backend_ready,
         native_generation,
         native_value,
         native_private,
         native_size,
         applied_valid,
         applied_session,
         applied_handoff_epoch
       >>

PrepareTarget ==
    /\ ActiveHandoff
    /\ PendingMatchesHandoff
    /\ ~handoff.target_prepared
    /\ HandoffProcessesFresh
    /\ backend_ready[handoff.target]
    /\ (handoff.target = SERVER_HOST \/ channel_up)
    /\ handoff' = [handoff EXCEPT
         !.target_prepared = TRUE,
         !.target_baseline = native_generation[handoff.target]
       ]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, wire, stage, retired,
                    applied_valid, applied_session, applied_handoff_epoch>>

PrepareFailure ==
    /\ ActiveHandoff
    /\ PendingMatchesHandoff
    /\ ~handoff.target_prepared
    /\ (\/ ~HandoffProcessesFresh
        \/ ~backend_ready[handoff.target]
        \/ (handoff.target = REMOTE_HOST /\ ~channel_up))
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

CaptureSuccess ==
    /\ handoff.phase = "capturing"
    /\ process_session[handoff.source] = handoff.source_process
    /\ backend_ready[handoff.source]
    /\ native_generation[handoff.source] = handoff.source_start_generation
    /\ ~native_private[handoff.source]
    /\ native_size[handoff.source] <= MAX_PAYLOAD
    /\ handoff' = [handoff EXCEPT
         !.phase = "ready",
         !.snapshot_kind = IF native_value[handoff.source] = "empty" THEN "empty" ELSE "text",
         !.snapshot_value = native_value[handoff.source],
         !.snapshot_size = native_size[handoff.source],
         !.snapshot_private = FALSE
       ]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, wire, stage, retired,
                    applied_valid, applied_session, applied_handoff_epoch>>

RetrySourceChanged ==
    /\ handoff.phase = "capturing"
    /\ native_generation[handoff.source] # handoff.source_start_generation
    /\ handoff.retries < MAX_RETRIES
    /\ SourceStillCurrent
    /\ handoff' = [handoff EXCEPT
         !.source_start_generation = native_generation[handoff.source],
         !.retries = @ + 1
       ]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, wire, stage, retired,
                    applied_valid, applied_session, applied_handoff_epoch>>

CaptureFailure ==
    /\ handoff.phase = "capturing"
    /\ (\/ process_session[handoff.source] # handoff.source_process
        \/ ~backend_ready[handoff.source]
        \/ native_private[handoff.source]
        \/ native_size[handoff.source] > MAX_PAYLOAD
        \/ /\ native_generation[handoff.source] # handoff.source_start_generation
              /\ (handoff.retries = MAX_RETRIES \/ ~SourceStillCurrent))
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

SendSnapshot ==
    /\ handoff.phase = "ready"
    /\ handoff.target_prepared
    /\ HandoffRelevant
    /\ channel_up
    /\ ~wire.pending
    /\ handoff' = [handoff EXCEPT !.phase = "in_flight"]
    /\ wire' = WireFromHandoff(handoff)
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, stage, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

TransferFailure ==
    /\ (\/ /\ handoff.phase = "ready"
              /\ handoff.target_prepared
              /\ ~channel_up
        \/ /\ handoff.phase = "in_flight"
              /\ ~channel_up)
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

CorruptWire ==
    /\ wire.pending
    /\ wire.integrity
    /\ wire' = [wire EXCEPT !.integrity = FALSE]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, handoff, stage, retired,
                    applied_valid, applied_session, applied_handoff_epoch>>

DeliverRetired ==
    /\ clipboard_enabled
    /\ retired.valid
    /\ ~wire.pending
    /\ wire' = WireFromRetired(retired)
    /\ retired' = [retired EXCEPT !.valid = FALSE]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, handoff, stage, applied_valid,
                    applied_session, applied_handoff_epoch>>

ReceiveSnapshot ==
    /\ wire.pending
    /\ wire.integrity
    /\ WireMatchesActiveHandoff
    /\ HandoffRelevant
    /\ handoff.target_prepared
    /\ wire.snapshot_size <= MAX_PAYLOAD
    /\ SnapshotKindMatches(wire.snapshot_kind, wire.snapshot_value, wire.snapshot_size)
    /\ ~AlreadyApplied(wire.target, wire.target_authority, wire.handoff_epoch)
    /\ handoff' = [handoff EXCEPT !.phase = "staged"]
    /\ stage' = StageFromWire(wire, handoff.target_baseline)
    /\ wire' = EmptyWire
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

RejectSnapshot ==
    /\ wire.pending
    /\ (\/ ~wire.integrity
        \/ ~WireMatchesActiveHandoff
        \/ ~HandoffRelevant
        \/ ~handoff.target_prepared
        \/ wire.snapshot_size > MAX_PAYLOAD
        \/ ~SnapshotKindMatches(wire.snapshot_kind, wire.snapshot_value, wire.snapshot_size)
        \/ AlreadyApplied(wire.target, wire.target_authority, wire.handoff_epoch))
    /\ handoff' = IF WireMatchesActiveHandoff
                      THEN [handoff EXCEPT !.phase = "skipped"]
                      ELSE handoff
    /\ wire' = EmptyWire
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, stage, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

ActivateTarget ==
    /\ ActiveHandoff
    /\ TargetIsCurrent
    /\ HandoffProcessesFresh
    /\ handoff.target_prepared
    /\ ~handoff.target_activated
    /\ (handoff.target = SERVER_HOST \/ channel_up)
    /\ handoff' = [handoff EXCEPT !.target_activated = TRUE]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, wire, stage, retired,
                    applied_valid, applied_session, applied_handoff_epoch>>

ActivationFailure ==
    /\ ActiveHandoff
    /\ TargetIsCurrent
    /\ handoff.target_prepared
    /\ ~handoff.target_activated
    /\ (\/ ~HandoffProcessesFresh
        \/ /\ handoff.target = REMOTE_HOST
              /\ ~channel_up)
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

SkipUnpreparedAfterCommit ==
    /\ ActiveHandoff
    /\ TargetIsCurrent
    /\ ~handoff.target_prepared
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

ApplySnapshot ==
    /\ handoff.phase = "staged"
    /\ stage.valid
    /\ StageMatchesHandoff
    /\ TargetIsCurrent
    /\ HandoffProcessesFresh
    /\ handoff.target_prepared
    /\ handoff.target_activated
    /\ backend_ready[stage.target]
    /\ native_generation[stage.target] = stage.target_baseline
    /\ stage.snapshot_size <= MAX_PAYLOAD
    /\ SnapshotKindMatches(stage.snapshot_kind, stage.snapshot_value, stage.snapshot_size)
    /\ ~AlreadyApplied(stage.target, stage.target_authority, stage.handoff_epoch)
    /\ native_generation' = [native_generation EXCEPT
         ![stage.target] = BumpGeneration(@)
       ]
    /\ native_value' = [native_value EXCEPT ![stage.target] = stage.snapshot_value]
    /\ native_private' = [native_private EXCEPT ![stage.target] = FALSE]
    /\ native_size' = [native_size EXCEPT ![stage.target] = stage.snapshot_size]
    /\ applied_valid' = [applied_valid EXCEPT ![stage.target] = TRUE]
    /\ applied_session' = [applied_session EXCEPT
         ![stage.target] = stage.target_authority
       ]
    /\ applied_handoff_epoch' = [applied_handoff_epoch EXCEPT
         ![stage.target] = stage.handoff_epoch
       ]
    /\ handoff' = [handoff EXCEPT !.phase = "applied"]
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, wire, retired>>

DropStage ==
    /\ handoff.phase = "staged"
    /\ stage.valid
    /\ (\/ ~StageMatchesHandoff
        \/ /\ ~PendingMatchesHandoff
              /\ ~TargetIsCurrent
        \/ /\ TargetIsCurrent
              /\ (\/ ~HandoffProcessesFresh
                  \/ ~backend_ready[stage.target]
                  \/ native_generation[stage.target] # stage.target_baseline
                  \/ AlreadyApplied(stage.target, stage.target_authority,
                                    stage.handoff_epoch)))
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, wire, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

StaleHandoffFailure ==
    /\ ActiveHandoff
    /\ ~PendingMatchesHandoff
    /\ ~TargetIsCurrent
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

HandoffProcessFailure ==
    /\ ActiveHandoff
    /\ ~HandoffProcessesFresh
    /\ handoff' = [handoff EXCEPT !.phase = "skipped"]
    /\ wire' = EmptyWire
    /\ stage' = EmptyStage
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, nativeVars, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

SetChannel(available) ==
    /\ available \in BOOLEAN
    /\ channel_up # available
    /\ channel_up' = available
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, backend_ready,
                    nativeVars, handoff, wire, stage, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

SetBackend(host, available) ==
    /\ host \in Host
    /\ available \in BOOLEAN
    /\ backend_ready[host] # available
    /\ backend_ready' = [backend_ready EXCEPT ![host] = available]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    nativeVars, handoff, wire, stage, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

NativeChange(host, value, private, size) ==
    /\ host \in Host
    /\ value \in Contents
    /\ private \in BOOLEAN
    /\ size \in PayloadSize
    /\ WellFormedContent(value, size)
    /\ native_generation[host] < MAX_GENERATION
    /\ native_generation' = [native_generation EXCEPT ![host] = @ + 1]
    /\ native_value' = [native_value EXCEPT ![host] = value]
    /\ native_private' = [native_private EXCEPT ![host] = private]
    /\ native_size' = [native_size EXCEPT ![host] = size]
    /\ UNCHANGED <<inputVars, process_session, clipboard_enabled, channel_up,
                    backend_ready, handoff, wire, stage, retired, applied_valid,
                    applied_session, applied_handoff_epoch>>

PeerRestart ==
    /\ process_session[REMOTE_HOST] < MAX_SESSION
    /\ process_session' = [process_session EXCEPT ![REMOTE_HOST] = @ + 1]
    /\ channel_up' = FALSE
    /\ backend_ready' = [backend_ready EXCEPT ![REMOTE_HOST] = FALSE]
    /\ UNCHANGED <<inputVars, clipboard_enabled, nativeVars, handoff, wire,
                    stage, retired, applied_valid, applied_session,
                    applied_handoff_epoch>>

InputSettlement == CommitSwitch \/ AbortSwitch

InternalClipboardProgress ==
    PrepareTarget
    \/ PrepareFailure
    \/ CaptureSuccess
    \/ RetrySourceChanged
    \/ CaptureFailure
    \/ SendSnapshot
    \/ TransferFailure
    \/ ReceiveSnapshot
    \/ RejectSnapshot
    \/ ActivateTarget
    \/ ActivationFailure
    \/ SkipUnpreparedAfterCommit
    \/ ApplySnapshot
    \/ DropStage
    \/ StaleHandoffFailure
    \/ HandoffProcessFailure

ClipboardFailure ==
    PrepareFailure
    \/ CaptureFailure
    \/ TransferFailure
    \/ RejectSnapshot
    \/ ActivationFailure
    \/ SkipUnpreparedAfterCommit
    \/ DropStage
    \/ StaleHandoffFailure
    \/ HandoffProcessFailure

ClipboardNext ==
    InternalClipboardProgress
    \/ CorruptWire
    \/ DeliverRetired
    \/ (\E enabled \in BOOLEAN : SetClipboardEnabled(enabled))
    \/ (\E available \in BOOLEAN : SetChannel(available))
    \/ (\E host \in Host :
          \E available \in BOOLEAN : SetBackend(host, available))
    \/ (\E host \in Host :
          \E value \in Contents :
            \E private \in BOOLEAN :
              \E size \in PayloadSize : NativeChange(host, value, private, size))
    \/ PeerRestart

InputNext ==
    BeginRemote
    \/ BeginFallback
    \/ CommitSwitch
    \/ AbortSwitch
    \/ (\E host \in Host :
          \E ready \in BOOLEAN : SetInputReady(host, ready))
    \/ RestartAuthority

Next == InputNext \/ ClipboardNext

CommitEnabledWithoutClipboard ==
    (switch_phase = "pending" /\ input_ready[switch_target]) => ENABLED CommitSwitch

FallbackBeginEnabledWithoutClipboard ==
    (switch_phase = "idle"
     /\ input_owner = REMOTE_HOST
     /\ input_ready[SERVER_HOST]
     /\ next_epoch < MAX_EPOCH) => ENABLED BeginFallback

SafetySpec == Init /\ [][Next]_vars

Spec == SafetySpec
        /\ WF_vars(InputSettlement)
        /\ WF_vars(InternalClipboardProgress)

\* Clipboard and clipboard-environment actions cannot mutate ownership or the
\* input transition. Native user clipboard changes are included deliberately.
InputIndependence ==
    [][~ClipboardNext \/ UNCHANGED inputVars]_vars

\* Every protocol/native clipboard failure leaves all native clipboard values
\* and generations unchanged. Explicit Empty is handled only by ApplySnapshot.
FailurePreservesNativeClipboard ==
    [][~ClipboardFailure \/ UNCHANGED nativeVars]_vars

ConfigurationPreservesNativeClipboard ==
    [][~(\E enabled \in BOOLEAN : SetClipboardEnabled(enabled))
       \/ UNCHANGED nativeVars]_vars

EventuallySwitchSettles ==
    (switch_phase = "pending") ~> (switch_phase = "idle")

\* Epoch/session bounds make user-generated supersession finite in this model.
\* Fair internal outcomes then force every active handoff out of an active phase.
EventuallyClipboardSettles ==
    ActiveHandoff ~> ~ActiveHandoff

\* TLC scenario constraints reduce independent environment dimensions without
\* removing any individual clipboard failure class. The full production spec
\* remains Spec; these predicates are used only by finite checker profiles.
TLCDeepState ==
    /\ clipboard_enabled
    /\ input_ready = [h \in Host |-> TRUE]
    \* One native change is enough to cover either source-read or target-apply
    \* races in separate behaviors.
    /\ native_generation[SERVER_HOST] + native_generation[REMOTE_HOST] <= 1
    \* Source and target backend failure remain independently reachable.
    /\ backend_ready[SERVER_HOST] \/ backend_ready[REMOTE_HOST]
    \* Authority restart and peer restart remain independently reachable;
    \* their irrelevant simultaneous cross-product is omitted.
    /\ authority_session + process_session[REMOTE_HOST] <= 1

TLCCapabilityDisabledState ==
    /\ ~clipboard_enabled
    /\ channel_up
    /\ input_ready = [h \in Host |-> TRUE]
    /\ backend_ready = [h \in Host |-> TRUE]
    /\ native_generation = [h \in Host |-> 0]
    /\ authority_session = 0
    /\ process_session = [h \in Host |-> 0]

=============================================================================
