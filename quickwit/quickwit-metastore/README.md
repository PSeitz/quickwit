# quickwit-metastore

## Ingesthree file registration for indexing

`RegisterFileForIndexing` records an immutable uploaded file for indexing and returns its
`DocumentsStagedForUpload` record. The record tracks the documents through processing
and retains completion evidence. The API is implemented by the PostgreSQL and
file-backed metastores and exposed through the metastore gRPC service.
The ingester calls it after a confirmed upload.

- Identity is `file_id`, a globally unique canonical ULID retained across recovery
  and retries. The index UID associates the file with its index incarnation.
- The caller supplies the storage URI and positive file size, including framing.
  Sizes are limited to `INT64_MAX` in both backends. URIs use an explicit storage
  protocol (S3, Azure, GCS, local file, or RAM storage).
- Call only after upload succeeds. Registration trusts the caller's assertion and
  does not query object storage.
- A new record has a metastore creation timestamp and no processing or completion
  timestamp. Repeated registration returns the existing record, preserving every
  field. The caller keeps index UID, URI, and size unchanged across retries.
  Registration uses the stored metadata without comparing it with the request,
  including for completed files.
- A processing timestamp without completion represents in-flight work. A completion
  timestamp represents retained completion evidence. Registration cannot set or
  reset either timestamp. Assignment and publication use the APIs below.
- Missing or deleted index incarnations return `NotFound`. Recreating the same index
  ID does not allow registration against its old UID. Deleting an index removes its
  staged-file records, as it does other index-owned metadata. S3 cleanup remains separate.

PostgreSQL migration `30_create-staged-files` adds the `staged_files` table with
`file_id` as its primary key. `index_uid` remains a required foreign key. Registration
holds a shared key lock on the index through commit and uses `INSERT ... ON CONFLICT
(file_id) DO NOTHING`, followed by a separate read by file ID. The read sees a
concurrently committed registration under READ COMMITTED. Retries never update
existing rows. PostgreSQL enforces file ID uniqueness across indexes. The file-backed
metastore keeps a map keyed by file ID within each index and relies on callers to
generate globally unique IDs. It persists records with the index and skips writes on retries.
No completion-evidence expiry is implemented.

Focused tests, with `QW_TEST_DATABASE_URL` pointing to a test PostgreSQL database:

```sh
cargo nextest run -p quickwit-metastore --features postgres register_file_for_indexing
```

## Ingesthree assignment and publication

`AssignFilesForIndexing` atomically claims eligible inputs for one index incarnation.
It selects the globally oldest eligible file, then a same-index prefix ordered by
registration time in Unix seconds and file ID. Eligible means unfinished and either
never assigned or started strictly before metastore time minus `retry_after_secs`.
The retry delay must be positive and should be configured consistently across workers.
It controls duplicate work versus recovery delay, not an exclusive publication right.

Requests specify `max_num_files` (1–1000) and `max_num_bytes` (1–INT64_MAX).
The oldest file is always included, even if it exceeds the byte target. Additional
files stop at the first one that would exceed the target. Claims never wait to fill
a group. An empty response means no eligible work was available. Losing an assignment
response leaves work recoverable after the retry threshold without an assignment journal.

`PublishFileSplits` takes the index UID, 1–1000 distinct input file IDs, up to 1000
distinct staged output split IDs, and a canonical ULID `publication_id` generated
per attempt. Empty output is valid. It publishes all outputs and completes all inputs
atomically. The first commit wins, including an older attempt after reassignment.
If any input is already completed by another publication, the entire group loses
without changing its remaining inputs or outputs. Missing inputs or invalid output
states fail the operation without partial changes.

The caller must keep the publication ID and exact input/output sets unchanged across
retries, and use different output split IDs for different attempts. `published: true`
means this attempt won or is retrying its winning commit. `published: false` means
another publication completed at least one input. Errors and lost responses are
ambiguous: retry the same request before deciding whether to clean up outputs.
Completion records retain the winning publication ID, so this works even after
published splits have been merged or deleted. The ID identifies the committed result,
not an assignment lease. No additional publication journal is stored.

PostgreSQL migration `31_add-file-publication` adds the winner ID and global/per-index
assignment indexes excluding completed history. It normalizes existing unfinished
registration timestamps to whole seconds and uses that precision for new registrations,
matching the API and file-backed ordering. Assignment uses `FOR UPDATE SKIP LOCKED`,
so concurrent claims can skip locked inputs. Publication locks the index incarnation,
then input and output rows in consistent ID order, and commits both state changes in
one transaction. Staging and split cleanup use the same output lock order.

The file-backed backend retains its single-writer restriction. Assignment takes an
exclusive mutation guard through its global scan and persistence, blocking mutations
on all indexes. Outside assignment/deletion, ordinary mutations of independent indexes
can run concurrently. Index deletion waits for in-flight mutations across indexes.
Once an index mutation starts persistence, a task retains its locks and
finishes the storage write and cache reconciliation even if the caller cancels.
This applies to the shared mutation path because staging, registration, and cleanup
also write whole-index snapshots. It protects first-publication-wins while the runtime
remains running. Process restart reloads stored metadata.

Tests cover concurrent claims/publications, overlapping groups, slow older winners,
validation rollback, persisted winner evidence after split deletion, lost write
responses, and cancellation during storage writes. PostgreSQL fault injection verifies
rollback when input completion fails after the output update. Ingesthree indexing workers
end at durable publication. The standalone compaction planner discovers their published
splits through metastore scans. Completion-evidence expiry remains a separate
implementation step.

```sh
cargo nextest run -p quickwit-metastore --features postgres
```

## Completed ingesthree input cleanup

`ListCompletedFiles` reads completed inputs globally across indexes, ordered by
`file_id` ascending. `after_file_id` is an exclusive keyset cursor. An empty cursor
starts a scan. `max_num_files` must be between 1 and 1000. The response contains
`DocumentsStagedForUpload` records with a completion timestamp and file IDs strictly
greater than the cursor. Unfinished inputs are excluded, including assigned inputs.
PostgreSQL migration `32_add-completed-files-index` adds a partial file-ID index
covering completed records.

Janitor uses these records to delete completed input objects. Listing is read-only
and retains completion timestamps and winning publication IDs. It adds no deletion
or cleanup state. Repeated registration still returns the completed record unchanged.
Callers restart whole scans after reaching the end. Repeated deletion intentionally
catches late reuploads, including an uploader crashing before it retries registration.
A scan is not a snapshot across pages. Completions behind its cursor are discovered
on the next whole scan. Index deletion still removes its index-owned records.

```sh
cargo nextest run -p quickwit-metastore --features postgres list_completed_files
```

## Starting postgres

The following command starts a postgresql server
locally to test the postgres metastore implementation.

`docker-compose up postgres`

Its data is saved in the tmp directory, and
is not necessarily cleaned up between two runs.

You can execute `make rm-postgres` to remove the
data of this postgresql database.

## Testing quickwit-metastore

To test FileBackedMetastore only, use the following command.

```
$ cargo test
```

To test including PostgresqlMetastore, you need to start PostgreSQL beforehand.
Start PostgreSQL for testing with the following command in `quickwit` project root.

```
$ make docker-compose-up DOCKER_SERVICES=postgres
```

Once PostgreSQL is up and running, you can run tests including PostgresqlMetastore with the following command.

```
$ cargo test --features=postgres
```

You can stop PostgreSQL with the following command.

```
$ docker-compose down
```

## Sqlx-cli and migrations

This sqlx-cli can be useful (but is not necessary) to work with migrations.

```
cargo install sqlx-cli
```

You can then use the following commands to apply/revert your postgresql migrations.
```
sqlx migrate run  --database-url postgres://quickwit-dev:quickwit-dev@localhost:5432/quickwit-metastore-dev --source migrations/postgresql
sqlx migrate revert  --database-url postgres://quickwit-dev:quickwit-dev@localhost:5432/quickwit-metastore-dev --source migrations/postgresql
```

## Deferred migrations

`migrations/postgresql_deferred` holds long-running, degrade-gracefully migrations (e.g. `CREATE INDEX CONCURRENTLY`).
They run in a background task after readiness, elected across pods by a Postgres advisory lock, and share the
`_sqlx_migrations` table with the required track, so version numbers must be globally unique across both dirs and each
migration must be idempotent. See `migrations/postgresql_deferred/README.md` for authoring rules.
```
sqlx migrate run  --database-url postgres://quickwit-dev:quickwit-dev@localhost:5432/quickwit-metastore-dev --source migrations/postgresql_deferred
```
