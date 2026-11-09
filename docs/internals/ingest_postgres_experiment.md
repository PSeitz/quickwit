# PostgreSQL ingest metadata experiment — 2026-11-08

## Finding and scope

The synthetic metadata workload achieved **100 registrations/s and 99.956–100 completed files/s** with **259.2 million retained completed-file records**, under an **8-CPU / 32-GiB PostgreSQL container limit**. All workload transactions succeeded. This is positive evidence for the proposed registration, assignment and conditional-publication transaction shape. It measures neither real indexing throughput nor the complete Quickwit metastore.

The experiment follows the [file-group publication contract](../../design_answers.md#file-assignment-and-publication) and the [metadata sizing target](../../design_answers.md#postgresql-capacity-and-requirements). Execution and measurements were supplied by Fast Fiasco. The full report, runnable scripts and raw evidence are committed together in the benchmarks repository.

## Durable artifacts

- Repository: `quickwit-oss/benchmarks`.
- Commit: **`09d9bf1878846b120f0761a5f3fbef3dd461e6ae`**.
- Directory: `tools/ingest-postgres/`.
- Report: `report.md`. Protocol/reproduction: `README.md`.
- Reproduction code: `schema.sql`, `bench.py`, `run.sh`, `maintenance.py`, `summarize.py`.
- Derived results: `summary.json`, `maintenance-summary.json`.
- Raw evidence: `raw-results-20261108.tar.gz`, **14,109,858 bytes**.
- Archive SHA-256: **`9e74347f5ee1746ed466597231e7e399e9add1f0b1cab13efe0695469679bd18`**.

The commit is stored locally; this recording task did not push it. Once published, its [immutable artifact location](https://github.com/quickwit-oss/benchmarks/tree/09d9bf1878846b120f0761a5f3fbef3dd461e6ae/tools/ingest-postgres) resolves through GitHub. The Git object itself already retains the report and archive:

```bash
git -C /path/to/benchmarks show \
  09d9bf1878846b120f0761a5f3fbef3dd461e6ae:tools/ingest-postgres/report.md
```

The archive separates the main run (`results/20261108T133048Z-full/`) and maintenance follow-up (`results/20261108-maintenance/`) from smoke tests and the abandoned shuffled-seed preparation. It contains transaction JSONL, resource samples, SQL snapshots/plans, PostgreSQL logs/notices/settings, image identity and as-run scripts. Preliminary runs are not full-scale validation results.

## Environment and workload

- **Remote execution:** existing EC2 `52.4.215.42`, not the Mac. The host was `m8azn.metal-12xl`, with 48 exposed cores and approximately 188 GiB RAM. PostgreSQL used CPUs 0–7 with an 8-CPU quota, 32 GiB memory and no additional swap. The client used CPUs 8–11.
- **Database:** isolated PostgreSQL **18.6** container. `fsync`, `full_page_writes` and `synchronous_commit` were enabled. PostgreSQL shared buffers were 8 GiB. Client connections were pooled at 8–32 and released during simulated processing.
- **Storage:** existing **3,000-GiB EBS root volume**, not a new 1-TB volume. Terraform state recorded gp3, 15,360 IOPS and 960 MiB/s. Live `DescribeVolumes` was denied, so provisioned IOPS/throughput were not independently verified.
- **History:** 259.2M completed input rows, inserted chronologically. Historical split/publication rows were **not** seeded. The input relation occupied **87.18 GiB**, including indexes and auxiliary files, approximately **361 bytes/input** in this compact synthetic schema.
- **Load:** independent registrar at 100 files/s, with 1,000 simulated pipelines. Each 100-MB file incurred a ten-second processing sleep. A full ten-file same-index group slept for 100 seconds. No assignment renewals were used.
- **Cases:** single-file and ten-file groups, each with one hot index or 100 indexes. Each case ran once. Single-file phases lasted five minutes with 30-second warmup; grouped phases lasted seven minutes with 110-second warmup. Preloaded same-index backlog allowed full groups.

## Steady results

Latency includes client pool acquisition and transaction commit. Arrival scheduling lag was recorded separately. CPU is the mean database-container usage within the measured steady window.

| Files/group | Indexes | Registrations/s | Completed files/s | Claim p99, ms | Publication p99, ms | DB CPU cores |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 100.00 | 99.963 | 2.522 | 2.443 | 0.085 |
| 1 | 100 | 100.00 | 99.956 | 2.985 | 2.451 | 0.145 |
| 10 | 1 | 100.00 | 100.000 | 7.161 | 4.095 | 0.047 |
| 10 | 100 | 100.00 | 100.000 | 5.221 | 4.313 | 0.041 |

Single-file unfinished queues grew by approximately ten files over 265 sampled seconds. Exactly 1,000 pipelines sleeping ten seconds/file leave no capacity margin for metadata work, yielding roughly a **0.04% throughput deficit**. This supports low metadata overhead, rather than perfectly stable queues at 100% modeled processing utilization.

Steady pool-wait p99 was below 0.008 ms. Busy startup produced claim/publication p99 around 149 ms, mostly client-pool queueing. Idle backoff settled at 266 empty claims/s with 0.412-ms p99. Low steady database CPU does not establish a minimum CPU/RAM configuration or maximum throughput.

## Maintenance finding

Input autovacuum with `INDEX_CLEANUP AUTO` bypassed **237,531 dead item identifiers**. Partial-index predicates exclude completed rows logically; old physical entries still need maintenance.

A separate 240-second baseline follow-up ran `VACUUM (VERBOSE, ANALYZE, INDEX_CLEANUP ON)` after evicting only the stopped experiment database's clean files from the filesystem cache. The cleanup:

- Removed **358,534 dead item identifiers**, making index pages reusable.
- Took **86.03 seconds** including analyze.
- Read approximately **30 GiB**, averaging **372 MiB/s** during the 80.1-second vacuum portion.
- Coexisted with **100 registrations/s**, **99.96 publications/s**, **2.765-ms publication p99** and zero workload errors during cleanup.

Explicit cleanup can coexist with this workload on the tested storage. Production maintenance cadence, sustained expiry, long-term bloat and performance on a default-throughput volume remain unvalidated.

## Correctness evidence and unresolved contract

The SQL assertions passed for an older reassigned attempt winning, overlapping groups having exactly one winner with no partial loser completion, recovery of the rejected group's remaining unfinished input, duplicate-publication rejection, completed-registration discard, expired-identity rejection, a registration/retirement race, and a controlled transaction-lock wait. The overlap fixture had no orphan output sets before synthetic retirement.

**The retirement fixture is not a production retirement design.** It embeds birth timestamps in synthetic identities and uses a cutoff that also rejects first-ever registrations of old files. The experiment does not establish that late first registrations can safely be discarded. The production safe-lateness and S3-cleanup contract remains unresolved.

## What this does not validate

- Actual `publish_splits`, metastore RPC/network hops, split staging, extra index/source-checkpoint locks, merges or competing production traffic.
- Full historical split/publication populations or real metadata widths.
- Actual S3 transfers, document indexing, small timeout-finished files, or natural group formation under sparse traffic.
- Large retry storms, replicas, backups/PITR, failover, crash recovery, multi-day maintenance equilibrium or a maximum-throughput sweep.
- A minimum **8-vCPU / 32-GB / 1-TB** production deployment. The tested host/storage, short durations and synthetic workload do not establish that claim.

The result supports the metadata transaction model at the stated 100-MB/file workload. Real metastore integration and a longer churn/expiry/storage test remain necessary.

## Evidence verification and retained remote state

Before recording these results, the archive checksum, safe extraction, Python/shell syntax and JSON parsing were checked locally. Both summary files were regenerated from archived phase/sample data. They matched except for backlog-slope floating-point roundoff of at most `3.56e-15` files/s. The archived driver and schema match the committed copies byte for byte. The committed launcher moves container shutdown before final log/inspect capture; its exact as-run predecessor remains in the archive. No database workload was rerun during this recording task.

The experiment report records the full container stopped with exit code 0 and no OOM. The benchmark worker timer was restored, and the existing metastore retained its original July 22 start time. Smoke/preliminary containers and data were removed. For follow-up, the stopped container `ingest-postgres-20261108T133048Z-full` and `/var/tmp/ingest-postgres-20261108T133048Z-full` remain on the remote host. Raw artifacts also remain under `/home/ubuntu/benchmarks/tools/ingest-postgres/`. These remote paths supplement the committed archive rather than serve as its only copy.
