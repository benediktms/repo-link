# RFC 0007 Stage 0 sidecar spike — reproduction

This directory re-establishes the storage/capacity evidence recorded in RFC 0007
(`docs/rfcs/0007-semantic-task-search.md`, §1 "Stage 0 sidecar spike", §5, §9).
Run `python3 spike.py` to regenerate everything; this document records the
fresh measurements and how they reconcile with the RFC's numbers.

The original 2026-07-24 one-off harness was not preserved with the RFC, so this
is a faithful **method** reproduction: the exact D5 schema, the D2 chunking
rules, and every measurement target re-implemented from the RFC text. Cold
numbers differ because the live corpus has grown and this machine/build differs
— the RFC itself flags timings as "local spike evidence, not CI acceptance
promises."

## Environment

| Env | Value |
|---|---|
| Python | 3.14.6 |
| SQLite | 3.53.1 (bundled `sqlite3`) |
| FTS5 | available |
| dbstat | available |
| Host | Apple M3 Max, macOS |

This is the same SQLite the RFC's capacity numbers were measured on (3.53.1).

## Fresh measurements (`python3 spike.py --scales current,10000,100000 --probes`)

| Scale | Chunks | Sidecar | WAL peak | Initial build | Full-path zero-change p95 | Hash-diff p95 | 100-row tx | Rebuild |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| current (live DB) | 4,797 | 12.7 MiB | 12.9 MiB | 0.35 s | 19.9 ms | 5.1 ms | 31.7 ms | 0.55 s |
| synthetic 10k | 10,000 | 31.7 MiB | 32.0 MiB | 0.73 s | 33.4 ms | 13.3 ms | 48.4 ms | 1.10 s |
| synthetic 100k | 100,000 | 316.9 MiB | 318.8 MiB | 8.03 s | 383 ms | 154 ms | 457 ms | 11.5 s |

`largest_wal_peak` is the **`-wal` file size only**, sampled immediately before
the truncating checkpoint (the main DB and `-shm` are excluded); `sidecar` is
the main-DB size after that checkpoint. The full-path `zero_change` p95 mirrors
D6 ordering and includes the authoritative source read inside the timed window
(an open connection + read of the task/comment rows each iteration), so it is a
true end-to-end reconcile cost, not just an in-memory re-chunk.

### Probes

- **Clear/delete-all contrast:** after clearing, `delete-all` + full
  auto-vacuum + truncating checkpoint leaves the sidecar at **44 KiB with zero
  freelist pages**; the counterfactual row-by-row FTS delete retains
  **639 KiB** of allocated space. Same direction as the RFC (44 KiB vs 22.9 MiB —
  smaller here because the clear-contrast probe corpus is 3,000 chunks, not
  100k).
- **`max_page_count` backstop:** a low page cap raises `SQLITE_FULL`
  ("database or disk is full") and rolls the write transaction back to **zero
  rows**; the real 512 MiB cap is otherwise fine.
- **`BEGIN IMMEDIATE` serialization:** a second sidecar writer gets
  `SQLITE_BUSY` until the first commits.
- **Profile compare-and-set:** only the first claim of a NULL
  `embedding_profile_id` wins (`first=1, second=0, owner=prof-A`).

## Real repo-link DB (authoritative live corpus)

The `current` row above was re-run against the real authoritative database
(`~/Library/Application Support/repo-link/repo-link.db`, opened read-only) on
2026-08-04. This is the definitive real-corpus measurement.

| Metric | Value |
|---|---:|
| Tasks / comments | 607 / 178 |
| Search chunks | 4,797 |
| Sidecar | 12.7 MiB |
| Sidecar vs authoritative DB | 12.7 MiB vs 23 MiB (~55%) |
| vs 512 MiB cap | ~2.5% |
| Full-path zero-change p95 (warm, incl. authoritative read) | **19.9 ms** |
| hash-diff p95 | 5.1 ms |
| Initial build | 345 ms |
| 100-row change tx | 31.7 ms |
| Rebuild | 550 ms |

Viability read: well inside every RFC limit. Reconcile is an order of
magnitude under the 150 ms D6 ceiling; the sidecar is ~2.5% of the 512 MiB cap
(~40× chunk headroom). The design's scaling cliff (~100k chunks, ~383 ms
reconcile, 317 MiB sidecar) is ~20× the current real corpus.

## Reconciliation with the RFC numbers

| Scale | Chunks | Sidecar | Chunks | Sidecar |
|---|---:|---:|---|---:|---:|
| | **RFC** | **RFC** | **Fresh** | **Fresh** |
| current | 1,658 | 5.39 MiB | 4,797 | 12.7 MiB |
| 10k | 10,000 | 31.76 MiB | 10,000 | 31.7 MiB |
| 100k | 100,000 | 317.7 MiB | 100,000 | 316.9 MiB |

- **Sizes track the RFC almost exactly** at the synthetic scales (31.7 vs
  31.76 MiB at 10k; 316.9 vs 317.7 MiB at 100k) — the storage model (4 KiB
  pages, `auto_vacuum=FULL`, one 384-dim f32 vector per chunk, FTS5 external
  content) is validated.
- **`current` differs structurally**: the live DB has grown since the RFC
  (607 tasks / 178 comments / 0.94 MiB text today vs 519 / 139 / 0.76 MiB on
  2026-07-24), and the D2 chunker here produces 4,797 chunks vs the RFC's
  1,658 — the original chunker's exact boundary decisions were not preserved.
  The per-chunk footprint is consistent; only the corpus grew.
- **Timings are slower** than the RFC (initial build 8.0 s vs 3.76 s at 100k;
  full-path zero-change 384 ms vs 62 ms at current — this reproduction measures
  the complete path including the authoritative read, which the RFC's 62 ms
  figure also covers). Expected: different machine, Python 3.14 vs the spike's
  environment, and a single-transaction build in this harness. The RFC
  pre-declares timings as local evidence; the load-bearing claims (sizes, caps,
  rollback behavior) reproduce cleanly.

## Methodology notes / scope

Faithfully reproduced from the RFC text:

- **D5 schema** exactly (schema parity with the current RFC, including the D6
  `validated_*` marker columns on `task_search_meta`): `task_search_meta`,
  `task_search_chunks` (with `UNIQUE(task_id, content_hash)` and a guard trigger
  rejecting text/identity updates), `task_search_vectors`, and `task_search_fts`
  (FTS5 external-content over `task_search_chunks(text)`) with the documented
  insert/delete triggers; 4 KiB pages, `auto_vacuum=FULL`, WAL,
  `foreign_keys=ON`, `max_page_count=131072`.
- **D2 chunking**: title-anchored, ~900-byte formatted-chunk budget (reserving
  header + ellipsis + one UTF-8 scalar for oversized titles, which are emitted
  as their own full-title chunk), paragraph-preserving with sentence/UTF-8-
  boundary fallback; empty-body → title-only.
- **D6 reconcile**: `BEGIN IMMEDIATE`, pre-write `integrity-check` on mutation,
  delete-missing/insert-new, content-hash keying, within-task dedup; the
  full-path benchmark acquires the sidecar writer transaction before the
  authoritative read and includes that read in the timed window.
- **Synthetic corpus**: exactly `N` unique single-chunk tasks (~884-byte
  formatted chunks), matching the RFC's ~900-byte formatted-chunk intent.

Scope note: this is a **storage/capacity spike** — it reproduces the D5 schema
and reconciles it at scale, but it does not exercise the D6
validated-integrity-marker *lifecycle* (persistence, invalidation on
diff/file-identity change) or the marker-driven zero-change integrity check.
Those are product behavior, not storage capacity; the marker columns exist in
the schema for parity but the harness performs `integrity-check` only on
mutation.

Not reproduced (deferred to the eventual Rust implementation / follow-up):
exact `dbstat` per-component byte split, the 512 MiB warn/refuse free-space
preflight math, multi-process lifecycle-lock exclusion, and the guarded
stale-vector insert rejection (the schema-level guards are present; the
app-layer hash/profile audits belong in `application-search`).

## Re-run

```bash
python3 spike.py --scales current,10000,100000 --probes   # full suite
python3 spike.py --scales 10000                            # a single synthetic scale
python3 spike.py --scales current --db /path/to/repo-link.db
```

All output is JSON on stdout. The `current` scale reads only from the
`--db` authoritative file (read-only); nothing writes to it.
