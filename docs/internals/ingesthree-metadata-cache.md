# Ingesthree intake metadata

Intake retains index/source settings and a compiled document mapper per index name.
Service clones share the cache. Successful batches reuse that information without a
metastore read or mapper construction. Concurrent misses for one index share a lookup.
Different indexes initialize independently. The metastore concurrency limit is unchanged.

A successful lookup checks the exact index incarnation, the built-in source's enabled
state and type, JSON input, and the absence of a source transform. The cached
configuration includes the index UID, all index settings (including mapping UID and
validation settings), and the built-in source. Validation-disabled indexes retain no
mapper. Index/source lookup failures and gRPC errors retain their existing response
semantics. Failed lookups are shared with their waiters and removed for subsequent retries.

## Changes and recovery

Primary metastore event layers publish successful create/update/delete index and
add/update/toggle/delete source operations. Subscribers publish a changing token in
Chitchat under `ingesthree_metadata_change`. This happens on metastore nodes even if
those nodes do not enable ingesthree. Observing a token change clears an intake cache.
The notification deliberately invalidates all cached indexes, keeping gossip to one
key per publishing node. Unrelated index mutations can therefore cause extra misses.

Lookup identity fences both invalidation and eviction: a completed lookup can return
its configuration only while it is still the lookup installed for that name. A late
completion cannot restore invalidated metadata. Requests already past resolution may
complete using the configuration they acquired. Inputs keep their exact index UID,
so deletion/recreation cannot retarget old inputs to the new incarnation.

The cache starts empty after restart. Production intake installs its gossip subscription
and background reconciliation before accepting traffic. Reconciliation starts every
minute after the previous pass completes, reads cached names in batches of 100 from
the primary metastore, and compares configurations. Unchanged configurations retain
their compiled mapper. Changed, deleted, failed, or unfinished lookups are evicted.
Reconciliation failures are logged and invalidate the affected batch, requiring later
intake to resolve through the metastore rather than silently keep unverifiable metadata.
This repairs missed notifications, including a mutation committed before an ambiguous
RPC error or a publisher restart. It also works during rolling upgrades before every
metastore publishes notifications.

The one-minute interval is a repair cadence, not a TTL or a guaranteed maximum stale
window. RPC latency and partitions affect convergence. Initialization and reconciliation
RPC work have a 30-second timeout including client readiness waits. Cache capacity is
1,024 index names. Completed least-recently-used lookups are evicted when capacity is
needed. If all lookups are pending, a new name receives resource exhaustion. Initialization
continues after caller cancellation, without retaining its document batch, so completion
and failed-lookup cleanup do not depend on a surviving caller. Service shutdown drops
its gossip listener and aborts reconciliation.

## Relationship to v2

V2 installs a mapper and validation policy when creating a shard and reuses them on
writes. Its mapper cache uses mapping UID. Index deletion schedules asynchronous
`RetainShards` synchronization; the deletion response does not wait for every ingester
to stop writes. V2 also has startup/control-plane synchronization and background shard
repair. Neither path establishes a per-write authoritative metadata freshness barrier.

Ingesthree keeps the reusable information per index instead of per shard. It refreshes
that information through mutation notifications and authoritative reconciliation.
It can update validation policy for subsequent batches without replacing a shard.
V2's existing shards retain their installed mapper/policy until their lifecycle replaces
them. Ingesthree files carry the index incarnation, not a pinned mapping version; file
indexing continues to resolve index configuration separately. That file-format behavior
predates this cache and remains unchanged.

Both paths can accept writes racing asynchronous deletion. An old-incarnation ACK does
not promise that data survives later index deletion. This change introduces no synchronous
mutation barrier and no new five-second freshness contract.
