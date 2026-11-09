# Ingesthree intake

This implementation includes the local-storage core, write queue, upload/registration,
and an opt-in service/router path. V2 remains the default.

## Enable the service

Set `ingest_api.enable_ingesthree: true` on public-router and ingesthree indexer nodes.
The existing ingest, Elasticsearch bulk, and enabled OTLP endpoints then use the file
router. Indexer nodes start gated intake recovery and one independently supervised
S3 indexing worker per configured CPU (`QW_NUM_CPUS`).
Ingesthree indexers require `enable_standalone_compactors: true`, with a standalone
planner on a Janitor node and merge workers on separate Compactor nodes. Router-only
nodes need no compaction opt-in. The planner discovers published splits through the
metastore, independently of the publishing indexer.
Local input uses `<data_dir>/ingesthree`. Uploaded input uses
`<default_index_root_uri>/.ingesthree/<file_id>` on storage readable by every indexer.
Keep this prefix unchanged while local inputs remain. Queue/disk budgets and shutdown
limits reuse `max_queue_memory_usage`, `max_queue_disk_usage`, and `decommission_timeout`.

Discovery includes an ingesthree capability and a node-wide `ingesthree_capacity`
score. Intake advertises capacity immediately and every five seconds using the v2
0–10 score, based on queue memory, retained disk bytes plus reservations, and their
recent change. Either budget at or below 5% remaining yields zero. The trend penalty
and integer scoring can yield zero earlier when headroom is shrinking. Closed intake
also advertises zero. The score measures buffer headroom, rather than CPU or MB/s.
Routers require ready ingesthree indexers with a positive score. Missing feedback is
ineligible until advertised. Among eligible nodes they prefer the same AZ, then stable
per-index affinity with a small local bonus. Positive scores do not change ranking.
They try up to three destinations on capacity/unavailability failures. Skipping a node
with zero/missing capacity consumes no attempt. Local admission remains authoritative
because gossip can lag and other routers can consume capacity. Capacity failures,
including all known destinations advertising zero, return HTTP 429. Saturation observed
through gossip or rejection is preserved if the other destinations are unavailable.
No ready destination with capacity feedback returns HTTP 503. File-presence hints remain
unimplemented. Requests never fall back to v2 or create shards.
The distinct internal RPC requires exactly one `ingest-version: ingesthree` metadata
value before any lookup or write. Public clients need no new header. Ingesthree nodes
expose this intake RPC instead of the v2 router RPC.

Public routing dispatches through `IngestRouterClient`. Ingesthree request and per-index
errors use `IngestServiceError`, preserving HTTP 400 for invalid input, 429 for overload,
and 503 for unavailability. Bulk responses retain successful items alongside failed items.
The intake RPC reuses existing request/success messages and index/source lookup failures.
Other intake failures use gRPC status codes. The v2 error enums and router remain unchanged.

Intake resolves current index/source configuration and validates documents. The source
must be the built-in JSON ingest source without a transform. Auto commits are supported.
Force, WaitFor, and legacy ingest are rejected explicitly. ACK means local append/kernel
flush, rather than fsync, upload, or search visibility.

Startup completes recovered handoffs before advertising readiness. Shutdown closes
intake, drains accepted files, and stops indexing restarts. A drain timeout retains local
data for recovery. Unexpected writer closure fails the server so process supervision
can restart it and recover retained files. V2 retains its existing retirement sequence.
See [node configuration](../../../../docs/configuration/node-config.md#experimental-ingesthree)
and [file indexing and compaction](../../../quickwit-indexing/src/ingesthree/README.md).

## Implemented

- `Ingester::open(data_dir, max_disk_bytes)` uses `<data_dir>/ingesthree`, with an
  exclusive directory lock held through writer destruction. Storage paths derive from
  the existing data directory and index-root settings.
- `append(index_uid, documents)` accumulates opaque, caller-validated documents in
  one active file per index incarnation. It returns the file ID after flushing the
  userspace buffer to the kernel. It does not fsync or promise host-crash survival.
- Each filename carries the file ULID. The header stores only the index UID,
  including the incarnation.
- The storage budget includes metadata, document bytes, and framing. Recovery
  counts retained files even when they exceed the current budget. Rejection before
  writing leaves the ingester usable. An IO failure stops further appends until
  reopen/recovery, preventing acknowledged records after an incomplete record.
- `Ingester::finish_file(index_uid)` closes the active file without changing its
  bytes. The next append uses a new file ID. `finish()` finishes all active files
  and releases the directory. These operations retain every input on disk.
- Startup checks and finishes surviving open files under their original IDs. New
  intake always uses fresh files. A complete unacknowledged record may survive,
  consistent with at-least-once delivery.

## Write queue

- `WriteQueue::open(data_dir, max_queue_bytes, max_disk_bytes, uploader)` recovers
  the core on the blocking pool. With `Some(Uploader::new(storage, metastore))`, it
  uploads, registers, and deletes every recovered local file before returning the
  queue. Registration delays therefore keep intake closed. `None` enables local-only writing.
  Clones share queue and disk byte limits. The queue passes document batches
  to the writer for validation and writing to per-index files.
- `append(index_uid, batch, mapper)` accepts the existing `DocBatchV2` payload.
  `Some(mapper)` reuses JSON/schema validation and per-document `ParseFailure`
  results. `None` corresponds to `validate_docs=false`. The existing
  `QW_DISABLE_DOCUMENT_VALIDATION` override is preserved. Index/configuration
  resolution remains the caller's responsibility.
- Validation runs through `run_cpu_intensive`. File IO runs through
  `spawn_blocking`. Only valid documents reach the core. An all-invalid batch
  returns its errors without creating a file.
- Admission reserves queue and disk bytes together before enqueueing. Queue-full,
  storage-full, invalid-request, and unavailable errors are distinct. The queue
  budget charges queued and processing input, descriptors, a possible filtered
  payload copy, and request overhead. Validator scratch, error-response allocations,
  allocator overhead, and externally shared backing buffers are outside this input
  budget.
- Disk reservations include framing and conservatively allow a new header for
  every queued request. After a successful append, reservations transfer atomically
  to retained file bytes. Validation failures release unused reservations.
  Recovery includes existing files in the disk budget.
- IO failure stops the worker and rejects pending/new work. Reopen recovers files
  and reconstructs accounting. `finish()` stops admission across all clones,
  drains the queue, and finishes files locally.

## Upload handoff

The writer polls one upload/registration future alongside file deadlines and incoming
batches. The future completes after registration. The writer handles local deletion
and its retries separately. Network waits leave the writer free to process requests.
Each finished file follows this sequence:

1. Stream its unchanged bytes through `Storage::put` to `<configured-prefix>/<file_id>`.
   The object key contains the bare ULID. Resolve storage at the complete upload prefix.
2. Call `RegisterFileForIndexing` with its index UID, file ID, full object URI, and
   framed byte size. Registration leaves any existing processing/completion state intact.
3. Delete the local file on the blocking pool, while the writer owns the directory.
4. Release the file's disk charge from both the core and shared admission accounting.

Failed uploads retry the same bytes and key. After a confirmed upload, only
registration retries. After confirmed registration, only deletion retries. Retryable
failures log their error and wait one second. Metastore connection/database/internal/IO
errors, timeouts, unavailability, and throttling retry. Authorization and missing-resource
upload errors fail immediately. Other storage errors retry because their categories
can include transient failures. S3 HTTP 403/404 errors retain these classifications,
except multipart `NoSuchUpload`, which allows replay after an ambiguous completion.

If registration reports that the index incarnation no longer exists, log an error and
discard that file. Delete its uploaded object, then its local copy, release its disk
charge, and continue with other files. This also applies during startup and drain.
Recreating the same index name never redirects old input to the new incarnation.
Remote deletion retries every second; an already-missing object counts as deleted.
Local files and disk charges remain until cleanup succeeds.

Other permanent registration errors stop the writer and close intake. Accepted local
data is finished and retained. Startup returns permanent upload or registration errors.
Drain returns operation errors. Background failures are logged.
Handoff processes one file at a time, so transient failures still hold up later files.

Upload progress is kept in memory. Restart replays retained files under their original
IDs, including files whose earlier upload or registration response was lost. Keep the
upload destination unchanged while retained files exist. Re-uploading a completed file
can recreate an object already deleted by remote cleanup. That cleanup protocol remains
unimplemented.

### Startup and shutdown

With an uploader, `open()` completes upload, registration, and local deletion for each
recovered file before exposing intake. Recovery processes one file at a time. Registration
or deletion delays block later recovery files and keep intake closed. After recovery,
the normal writer starts with fresh files and no recovered disk charges. Cancelling
startup retains remaining files and releases the directory after local filesystem work
completes. This replaces the earlier upload-only gate and deferred-registration queue.

`finish()` and dropping all queue handles stop handoff, drain queued appends, and finish
remaining files locally. They retain files whose deletion has not completed and return
local metadata on explicit `finish()`.

`drain()` stops admission across all clones, completes queued appends, finishes active
files, and waits for upload, registration, and local deletion. Existing handoff progress
is preserved. Success releases the directory lock and all disk charges, even while queue
handles remain alive. Drain requires an uploader. Without one, it finishes locally and
returns an error.

Dropping the drain future cancels network handoff and releases the directory after local
filesystem work completes. Admission stays closed and retained files remain available for
recovery. A caller can use `tokio::time::timeout` to limit shutdown waits. Reopen to resume
after cancellation or error. Cancellation can race with successful deletion or a remote
commit, so recovery always uses stable IDs. Local drain does not wait for indexing or
perform Janitor's remote-object cleanup.

## Finishing files

Finishing a file closes it and makes its contents immutable for upload.
The next append creates a fresh file with a new ID. The core implements this with
`finish_file(index_uid)`. Finished files use the `.finished` filename extension.

The internal defaults are **100 MB**, **10 seconds**, and **1,024 active files**.
No new node configuration is required.

- Size includes the header and document framing: 100 MB means 100,000,000 file
  bytes. The core finishes the file after a batch reaches the target. A batch
  stays in one file and can push it over the target.
- The deadline is fixed at creation plus 10 seconds. Appends leave it unchanged.
  The writer loop finishes files at their deadlines. It wakes for idle files and
  prioritizes expired deadlines over queued requests. A batch already being
  validated or written can finish in its current file after expiry. The loop
  finishes expired files before taking the next request.
- At the active-file limit, reject requests needing an additional writer with
  `IngestError::OpenFileLimit`. The writer checks this after validation, before
  file IO, and releases the request's queue/disk reservations on rejection.
  Existing writers continue. Finishing files at size/time limits allows new
  writers. Rejection leaves the worker running and existing files unchanged. The
  cap limits open input writers and their buffers, independently of retained disk usage.
- Finishing preserves bytes and disk charges. Later requests reserve a new header
  and receive a fresh file ID. Recovery finishes surviving open files under their
  original IDs. IO errors while finishing stop further writes until recovery.

## Experimental file format

```text
8-byte magic: QWINPUT followed by version byte 1
one framed metadata record: UTF-8 index UID
zero or more framed opaque documents

frame = u32 little-endian payload length
      + CRC32 of those four length bytes
      + payload
      + CRC32 of the payload
```

The index UID includes its incarnation (`index_id:incarnation_id`) and occupies
the entire metadata payload. Metadata is limited to 16 KiB.
Document lengths must fit in `u32`. CRC32 provides
accidental-corruption detection, not authentication. Recovery scans document
payloads with fixed memory rather than loading the input backlog into RAM.
The format is experimental and is not a released compatibility contract.
Recovery obtains the file ID from the filename.

File lifecycle:

1. Create `<id>.creating`, write and flush the header.
2. Rename to `<id>.open` before appending any documents.
3. Append framed documents and flush before returning success.
4. Finish by renaming to `<id>.finished`. Finished contents remain unchanged.

File creation and finishing use atomic renames, relying on unique file IDs and
exclusive directory ownership to keep destination paths unused.

Recovery removes `.creating` files: these can contain only headers, never
acknowledged documents. In an `.open` file it truncates only an incomplete final
record. It removes files with no complete documents. Bad checksums, unknown
formats, invalid metadata, and unexpected directory entries fail
recovery. Finished files are validated and never repaired. This protocol assumes
surviving storage and process failure, not external truncation or host/storage loss.

## Still required before production rollout

- Memory and physical scratch-space admission for indexing, and a progress policy for
  inputs exceeding the 1,000-output publication limit.
- Filesystem allocation/inode overhead, free-space checks, and drain/recovery
  reserves. The current logical byte budget is one component of disk admission.
- Cleanup after late uploads and index deletion after successful registration.
  Missing-index registration already discards the input and continues.
- Deployment/host-loss behavior, live AWS operation, and performance validation.
- Remote input/orphan cleanup and completion-evidence expiry.

Metastore registration, assignment, and atomic publication/completion are implemented in
both backends. See the [metastore contract](../../../quickwit-metastore/README.md#ingesthree-assignment-and-publication).

## Tests

The [TLA+ handoff model](tla/README.md) checks the proposed handoff, assignment,
publication, and cleanup protocol separately from the Rust implementation.

From the Rust workspace:

```bash
cargo nextest run -p quickwit-ingest ingesthree
```

Tests cover visibility before writer destruction, an abrupt subprocess exit that
bypasses destructors, every incomplete final-frame prefix, the metadata/first-document
recovery boundary, and corruption rejection. Property tests cover empty documents,
multi-chunk payloads, and corruption beyond the first chunk. Tests also cover
finished-file immutability, stable file identity, index-incarnation isolation,
logical disk-budget accounting, flush failure, and directory exclusion. Write-queue
tests cover concurrent admission, queued/processing reservations, recovered disk
usage, validation and malformed batches, worker failure, and draining. File-limit
tests cover exact size boundaries, oversized batches, idle and continuous traffic,
independent index deadlines, open-file rejection without eviction, disk reservations, restart,
drain, and IO failures. `handoff_tests.rs` covers handoff/capacity release, stage-specific
retries, registration replay, startup gating, queued/active drain, cancellation, and
permanent failures. Missing-index tests cover continued intake, index recreation,
startup/drain cleanup, and deletion retries. Tests inspect payloads, requests, retained
files, and usable capacity.
Metastore backend tests cover processing/completion preservation. Time-based tests use
a paused clock. Ingress/router tests cover metadata checks, source/schema validation,
capacity retries, discovery changes, and v2 separation. Running-server tests in
`quickwit-integration-tests/src/tests/ingesthree_tests.rs` exercise HTTP and ES bulk,
actual standalone compaction, searchability, mixed nodes, guarded RPCs, unsupported commits,
drain, and restart. `ingesthree_compaction_tests.rs` contains shared compaction assertions
and checks discovery after planner restart with the publishing indexer stopped.
Serve tests reject local-merge configuration on ingesthree indexers and verify that
writer failures trigger supervision while preserving accepted files.

`tests/ingesthree_s3.rs` uses the S3 client and persistent file-backed metastore. Its
standalone HTTP test stalls actual uploads and checks startup/drain cancellation and
connection closure. External tests verify remote bytes, keys, metadata, and local cleanup.
They kill child processes during upload, after an upload commits but before its response,
before registration, after registration commits but before its response, and after local
deletion. Restart must preserve identity, bytes, and any committed registration.

Run the external tests against a loopback S3-compatible server with an existing bucket:

```bash
INGESTHREE_S3_ENDPOINT=http://127.0.0.1:4566 \
INGESTHREE_S3_BUCKET=ingesthree-test AWS_EC2_METADATA_DISABLED=true \
cargo test -p quickwit-ingest --test ingesthree_s3 s3_ -- --ignored --nocapture
```

These tests passed with Moto 5.2.5 S3 emulation. They cover small single-PUT objects.
Live AWS, multipart crash windows, host/storage loss, remote cleanup races, and
end-to-end migration verification remain outside this evidence.
