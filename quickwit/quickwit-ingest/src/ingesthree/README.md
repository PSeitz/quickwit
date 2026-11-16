# Ingesthree intake

Experimental, opt-in ingestion through local files and object storage. V2 remains the default.

```text
request → validate → zstd-compress batch → append locally → ACK
                          ↓
                  finish file → upload → register → index
```

**ACK means the local write was flushed to the kernel.** It does not guarantee fsync,
host-loss durability, upload, or searchability. Delivery is at least once.

## Setup

- Set `ingest_api.enable_ingesthree: true` on routers and ingesthree indexers.
- Set `enable_standalone_compactors: true` on ingesthree indexers. Compactor workers
  handle later merges. Initial split publication works without them.
- Use the built-in JSON ingest source without a transform. Only `Auto` commits are supported.
- Existing ingest, Elasticsearch bulk, and enabled OTLP endpoints need no new client header.
  Internal RPCs require exactly one `ingest-version: ingesthree` value. There is no v2 fallback.

| Storage | Location |
| --- | --- |
| Local input | `<data_dir>/ingesthree` |
| Uploaded input | `<default_index_root_uri>/.ingesthree/<file_id>` |

All indexers need access to uploaded input. Keep the upload location unchanged while
local files remain. See [node configuration](../../../../docs/configuration/node-config.md#experimental-ingesthree).

## Intake and limits

- One writer per node validates, compresses, and appends batches serially. Validation
  and compression run on the CPU pool; file writes run on the blocking pool. Each
  index incarnation has its own active file. Queued batches are not combined.
- `validate_docs: false` skips document-schema validation. Batch-length checks remain.
  Invalid documents produce per-document errors. Valid documents continue through intake.
- Queue reservations cover input/validation buffers, document descriptors, serialization,
  and the maximum compressed allocation. Validator and zstd working memory are separate.
- Disk admission reserves the incompressible batch bound plus framing and a possible file
  header, then settles to bytes written. File-size targets and disk usage count compressed
  bytes. Filesystem overhead and recovery reserves remain separate.

| Limit | Default / setting |
| --- | --- |
| File size target | 100 MB; override with `QW_INGESTHREE_TARGET_UPLOAD_SIZE` |
| File age | 10 seconds from creation, checked between batches |
| Active files | 1,024 per node; existing files can still accept appends at the limit |
| Queue budget | `max_queue_memory_usage` |
| Disk budget | `max_queue_disk_usage` |

Files finish after a whole batch, so they can exceed the size target. Finishing keeps
the disk charge until local deletion. Invalid or zero size targets fail startup.

## Routing

Only ready ingesthree nodes with positive advertised capacity are eligible.

1. Try the router's own node first when eligible.
2. Otherwise, sample two peers and choose the higher capacity. Prefer the same AZ,
   then other AZs. Ties use per-index affinity.
3. Retry capacity/unavailability failures on up to three distinct destinations total.

Capacity is a 0–10 queue/disk-headroom score, advertised immediately and every five
seconds. It reaches zero at ≤5% remaining, earlier under growth, or when intake closes.
Local admission enforces the budgets. CPU usage is not part of the score.

Errors use **400** for invalid input, **429** for saturation, and **503** for unavailability.
Observed saturation takes precedence over unavailability. Bulk responses preserve partial successes.

## Upload and recovery

Finished files are uploaded, registered in the metastore, then deleted locally.
Disk charges remain until deletion succeeds.

- Retries and recovery reuse the file's ID and bytes. Restart can replay an upload or registration.
- If registration finds the index incarnation was deleted, discard that input.
  Recreating the same index name never redirects old documents to it.
- Permanent upload/registration errors close intake and retain local data for recovery.
  Local IO failures also require reopening before more writes.
- Recovery repairs incomplete final records in open files. Corruption fails recovery.
  Finished files are immutable and never repaired.

New files use `QWINPUT\x02`: each accepted batch is one zstd level-1 frame, with a
16-byte document-count/decoded-length header and 12-byte length/CRC32 framing.
Inside the compressed frame, each document has a 4-byte length and its opaque bytes.
The checksum covers the batch. Recovery drops a torn final batch in an open file,
and rejects corrupt complete batches. Readers validate the batch before yielding documents.
Incoming wire compression is decoded before validation; staging recompresses accepted documents.

Readers and recovery still support `QWINPUT\x01` files. Older binaries cannot read V2.
Stop intake and upgrade all ingesthree indexing workers before admitting V2 data.
Rollback requires draining V2 local and remote inputs with a V2-capable binary first.
See [file framing/recovery](documents_staged_for_upload.rs) and [batch codec](compressed_batch.rs).

## Startup and shutdown

| Operation | Result |
| --- | --- |
| `open()` with an uploader | Recover, upload/register or discard, and delete retained files before opening intake. |
| `finish()` or drop all handles | Stop intake, drain queued writes, and finish files locally. Upload completion is not required. |
| `drain()` | Stop intake and wait for queued writes, uploads, registration, and local deletion. Requires an uploader. |

- Cancellation or timeout retains remaining local files for recovery. Reopen to resume.
- Directory ownership lasts until local filesystem work finishes.
- Server shutdown uses `decommission_timeout`. Draining does not wait for indexing or remote cleanup.

## Metrics

Node-level metrics use the `quickwit_ingesthree_` prefix:

| Suffix | Meaning |
| --- | --- |
| `queue_bytes`, `queue_limit_bytes` | Reserved input bytes and queue budget |
| `disk_bytes`, `disk_reserved_bytes`, `disk_limit_bytes` | Retained file bytes, pending-write reservations, and disk budget |
| `capacity_score` | Last sampled 0–10 capacity score |
| `admission_rejections_total{reason}` | Rejections: `queue_full`, `storage_full`, `open_file_limit`, or `unavailable` |

Other useful metrics:

- `quickwit_ingest_docs_total` and `quickwit_ingest_docs_bytes_total`, labeled by validity:
  validation attempts before append. Retries can count again. These are not publication counters.
- `quickwit_ingest_ingest_result_total` and `quickwit_ingest_ingest_attempts`: routing outcomes and attempts.
- `quickwit_grpc_*` with `service="ingesthree"`: internal RPCs. RPC success can include subrequest failures.

Monitor physical disk space and process memory separately from the input-accounting gauges.

## Remaining rollout work

- Node-wide indexing memory/scratch admission and progress for oversized assignments.
- Physical disk/inode checks and recovery reserves.
- Remote input/orphan cleanup, late-upload races, and completion-evidence expiry.
- Live AWS, multipart crash windows, host-loss behavior, and performance validation.

## Tests and related docs

From the Rust workspace:

```bash
cargo nextest run -p quickwit-ingest ingesthree
```

External S3 tests require a loopback S3-compatible server and an existing bucket:

```bash
INGESTHREE_S3_ENDPOINT=http://127.0.0.1:4566 \
INGESTHREE_S3_BUCKET=ingesthree-test AWS_EC2_METADATA_DISABLED=true \
cargo test -p quickwit-ingest --test ingesthree_s3 s3_ -- --ignored --nocapture
```

- [Metadata cache](../../../../docs/internals/ingesthree-metadata-cache.md)
- [Indexing and compaction](../../../quickwit-indexing/src/ingesthree/README.md)
- [Metastore contract](../../../quickwit-metastore/README.md#ingesthree-assignment-and-publication)
- [TLA+ handoff model](tla/README.md)
