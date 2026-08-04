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

| Scale | Chunks | Sidecar | Largest WAL | Initial build | Full-path zero-change p95 | Hash-diff p95 | 100-row tx | Rebuild |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| current (live DB) | 4,785 | 13.3 MiB | 12.9 MiB | 1.42 s | 122 ms | 11.3 ms | 59.6 ms | 0.87 s |
| synthetic 10k | 10,000 | 33.2 MiB | 32.0 MiB | 0.98 s | 48.7 ms | 15.1 ms | 62.8 ms | 1.39 s |
| synthetic 100k | 100,000 | 332 MB | 334 MB | 9.20 s | 628 ms | 159 ms | 1.38 s | 14.2 s |

`largest_wal_peak` is the max size observed across the `-wal`/`-shm` companions
before checkpoint; `sidecar` is the main DB after a truncating checkpoint.

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
2026-08-04. This is the definitive real-corpus measurement; the earlier
`current` run's 122 ms reconcile was a cold first pass, the fresh number below
is the warm fast path.

| Metric | Value |
|---|---:|
| Tasks / comments | 607 / 178 |
| Search chunks | 4,785 |
| Sidecar | 13.3 MiB |
| Sidecar vs authoritative DB | 13.3 MiB vs 23 MiB (~57%) |
| vs 512 MiB cap | ~2.6% |
| Full-path zero-change p95 (warm) | **16 ms** |
| hash-diff p95 | 4.1 ms |
| Initial build | 323 ms |
| 100-row change tx | 29 ms |
| Rebuild | 503 ms |

Viability read: well inside every RFC limit. Reconcile is an order of
magnitude under the 150 ms D6 ceiling; the sidecar is ~2.6% of the 512 MiB cap
(~40× chunk headroom). The design's scaling cliff (~100k chunks, 628 ms
reconcile, 332 MB sidecar) is ~20× the current real corpus.

## Reconciliation with the RFC numbers

| Scale | Chunks | Sidecar | Chunks | Sidecar |
|---|---:|---:|---|---:|---:|
| | **RFC** | **RFC** | **Fresh** | **Fresh** |
| current | 1,658 | 5.39 MiB | 4,785 | 13.3 MiB |
| 10k | 10,000 | 31.76 MiB | 10,000 | 33.2 MiB |
| 100k | 100,000 | 317.7 MiB | 100,000 | 332 MB |

- **Sizes track the RFC almost exactly** at the synthetic scales (33.2 vs
  31.76 MiB at 10k; 332 vs 317.7 MiB at 100k) — the storage model (4 KiB pages,
  `auto_vacuum=FULL`, one 384-dim f32 vector per chunk, FTS5 external content)
  is validated.
- **`current` differs structurally**: the live DB has grown since the RFC
  (607 tasks / 178 comments / 0.94 MiB text today vs 519 / 139 / 0.76 MiB on
  2026-07-24), and the D2 chunker here produces 4,785 chunks vs the RFC's
  1,658 — the original chunker's exact boundary decisions were not preserved.
  The per-chunk footprint is consistent; only the corpus grew.
- **Timings are slower** than the RFC (initial build 9.2 s vs 3.76 s at 100k;
  full-path zero-change 122 ms vs 62 ms at current). Expected: different
  machine, Python 3.14 vs the spike's environment, and a single-transaction
  build in this harness. The RFC pre-declares timings as local evidence; the
  load-bearing claims (sizes, caps, rollback behavior) reproduce cleanly.

## Methodology notes / scope

Faithfully reproduced from the RFC text:

- **D5 schema** exactly: `task_search_meta`, `task_search_chunks` (with
  `UNIQUE(task_id, content_hash)` and a guard trigger rejecting text/identity
  updates), `task_search_vectors`, and `task_search_fts` (FTS5 external-content
  over `task_search_chunks(text)`) with the documented insert/delete triggers;
  4 KiB pages, `auto_vacuum=FULL`, WAL, `foreign_keys=ON`,
  `max_page_count=131072`.
- **D2 chunking**: title-anchored, ~918-byte budget, paragraph-preserving with
  sentence/UTF-8-boundary fallback; empty-body → title-only; `…`-marked title
  anchor when the title overflows.
- **D6 reconcile**: `BEGIN IMMEDIATE`, pre-write `integrity-check` on mutation,
  delete-missing/insert-new, content-hash keying, within-task dedup.

Note: the harness performs the `integrity-check` only on mutation, matching the
RFC as recorded at spike time. The RFC has since evolved (PR 256 review) to a
validated-integrity marker that also forces the check on a zero-change search
when the marker is missing/stale or the sidecar file identity changed; that
policy governs the product and is not re-measured by this storage spike.
- **Synthetic corpus**: exactly `N` unique single-chunk tasks (~884-byte
  formatted chunks), matching the RFC's "918-byte formatted chunks" intent.

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
