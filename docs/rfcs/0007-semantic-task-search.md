# RFC 0007 — Local task search over current task content

Status: Draft (2026-07-21, revised 2026-07-24)
Tracking epic: **#TBD**

## 1. Context

repo-link can filter and list tasks by structured fields, but it cannot find a
task from a concept expressed with different words. It also has no general
search command for exact phrases, identifiers, error strings, or code symbols.
The intended user-facing operation is therefore task search, with complementary
retrieval modes:

- literal matching should reliably find text the user typed exactly;
- lexical ranking should find shared words and phrases;
- semantic ranking should bridge relevant wording with little token overlap;
- all modes should return ordinary tasks rather than exposing index internals.

Dense embeddings close the semantic gap only within an important boundary:
**the model sees indexed task text and nothing else**. A task containing only an
opaque local identifier such as `RFC-0002` does not acquire the unpublished
meaning of that RFC. Clustering emerges when task titles, bodies, or comments
contain shared semantic context; vector search is direct similarity, not
transitive graph traversal.

### Task-only product boundary

repo-link manages tasks, not a general document collection. The search corpus is
limited to current task titles, bodies, and comments. Search does not introduce
repository-file, source-code, RFC, ADR, pull-request, or commit indexing.

The sidecar uses the term **search chunk** for one formatted piece of task
content. It deliberately does not use a generic `documents` table or API that
could imply a broader document-management feature.

### Current persistence shape

- `tasks.title` and `tasks.body` hold the current task summary and description.
- `task_comments` stores current synced and pending comments on a separate
  persistence axis; comment writes do not append task snapshots or change
  `sync_state`.
- `TaskRepository::list` deliberately skips comments, while point reads hydrate
  them.
- Synced comments are currently replaced wholesale, which changes their local
  surrogate IDs even when their remote identity and content are unchanged.
- `task_comments` currently stores `created_at` but not the remote comment's
  `updated_at`, even though GitHub comments are editable.
- `task_snapshots` contains append-only history and repeats title/body over
  time. Snapshot history is audit data, not search input.
- SQLite is authoritative for task state. The search index must remain a
  disposable projection that cannot affect task correctness or sync.

### Measured capacity

On 2026-07-24 the local database contained:

| Data | Count / size |
|---|---:|
| Current tasks | 519 |
| Current comments | 139 |
| Task snapshots, excluded from search | 3,952 |
| Current task title/body text | 0.76 MiB |
| Current title-anchored comment text | 0.15 MiB |
| Existing authoritative database | 18.75 MiB |

Ignoring long-content chunking, 658 current search units at 384 float32
dimensions require approximately 0.96 MiB of raw vectors:

```text
vector_bytes = search_chunks × dimensions × bytes_per_component
             = 658 × 384 × 4
             = 1,010,688 bytes
```

A conservative prototype split the current corpus into 1,150 chunks of at most
1,200 text bytes, stored 384-dimensional vectors, retained the formatted search
text, added task mappings, and built FTS5. After `VACUUM`, the complete sidecar
was 4.79 MiB:

| Prototype component | Size |
|---|---:|
| Raw vectors | 1.68 MiB |
| Formatted search text | 0.97 MiB |
| Complete SQLite sidecar including FTS5 and mappings | 4.79 MiB |
| Authoritative revision metadata with every task dirty | approximately 92 KiB |

The 1,200-byte split is a sizing proxy, not the production tokenizer rule. The
model, tokenizer, and real chunker are selected and re-measured in Stage 0.
These measurements establish the order of magnitude and a conservative current
baseline; the production path still enforces a preflight storage budget.

The embedding model itself is likely to consume more disk than the index.
Storage becomes dangerous through unbounded chunks, history retention, orphaned
content, retained model versions, or non-atomic rebuilds—not through current
task volume.

## 2. Goals

1. Search current task title, body, and comments using exact, lexical, and
   semantic retrieval.
2. Keep title and body together so the title anchors the description's meaning.
3. Preserve all indexed text when long bodies, comments, paragraphs, or titles
   require chunking; never truncate silently.
4. Detect task-content changes correctly without scanning the entire corpus on
   every warm search.
5. Keep model inference local and prevent ordinary search from downloading.
6. Bound authoritative metadata, sidecar growth, temporary rebuild space, and
   model-cache growth.
7. Keep the search index disposable, inspectable, rebuildable, and isolated
   from task correctness.
8. Reconcile one global current-task index; apply workspace, repo, and lifecycle
   filters only while ranking results.
9. Return task results with enough evidence to explain the winning match.
10. Preserve the existing architecture: domain rules remain pure, search
    orchestration lives in an application use case, and adapters stay behind
    ports.

## 3. Decisions

### D1 — Search only current task content

The v1 corpus is exactly:

- current task title;
- current task body; and
- current synced and pending comment bodies.

The index excludes task snapshots, audit events, outbox payloads, repository
files, RFC/ADR contents, issues not imported as tasks, pull requests, commits,
and source code.

Closed tasks remain searchable by default. Finding prior implementations and
decisions is a primary search use case; silently restricting results to open
work would remove much of the useful corpus. Workspace, repo, and lifecycle
filters are read from current authoritative task state and restrict eligible
task IDs at query time.

Search maintains one global current-task index across all workspaces. A scoped
query never creates, reconciles, prunes, or fingerprints a scoped sub-index.

### D2 — Title and body form the core search content

For a short task, construct one core search chunk with stable field labels:

```text
Title: <task.title>

Description:
<task.body>
```

Title and body are not embedded independently. The title supplies the task's
subject; the body supplies detail. Averaging or ranking independent title/body
vectors would discard that relationship.

The selected tokenizer determines the exact input budget, including model
special tokens and the fixed field labels. The chunker:

1. tokenizes the title and fixed labels;
2. packs complete body paragraphs while the formatted chunk fits;
3. splits an oversized paragraph at sentence boundaries and then tokenizer
   boundaries when necessary;
4. prepends the complete title to every body chunk when it fits; and
5. emits title-only chunks plus body chunks with the largest deterministic
   title anchor when the complete title itself exceeds the model budget.

The full title remains indexed even in the fifth case. A shortened body-chunk
anchor is explicitly marked in the formatted input; no title or body text is
silently discarded.

V1 uses no overlapping chunks. Paragraph-preserving boundaries plus title
anchoring retain context without duplicating an arbitrary overlap window.
Overlap may be added only if the evaluation fixture demonstrates boundary
misses.

An empty body still produces title-only core content. Chunk formatting and
boundary rules carry an explicit `chunk_format_version` in sidecar metadata;
changing them invalidates the sidecar and requires an explicit rebuild.

### D3 — Comments are separate, title-anchored search content

Each comment is formatted separately:

```text
Title: <task.title>

Comment:
<comment.body>
```

Author and timestamp are omitted from the indexed text because they are
metadata, not task meaning. A long comment follows D2's tokenizer-aware
chunking rule.

The implementation must not concatenate the entire comment thread into core
content. Threads can be long, repetitive, or operational; merging them would
dilute the task's primary meaning and make every comment change re-embed the
whole thread.

`task_comments.id`, the stable local surrogate, is always the search source ID.
The remote comment ID remains optional metadata. To make that identity stable:

- add `task_comments.updated_at`, backfilled from `created_at`;
- map the remote provider's comment update timestamp when present;
- upsert synced comments by `(task_id, remote_comment_id)`;
- preserve `task_comments.id` for an existing remote comment;
- update a pending row in place when its remote ID is assigned; and
- delete only remote comments that disappeared from the fetched remote set.

This replaces wholesale delete-and-reinsert comment persistence. It prevents
unchanged remote refreshes from generating false search invalidations and makes
edited comments observable without relying on creation timestamps.

### D4 — Hybrid exact, lexical, and semantic retrieval

`rl task search` is a general task-search command, not a dense-vector debugging
surface. V1 combines three retrieval lanes over eligible current task chunks:

1. **Literal lane.** Case-folded literal substring matches. A literal match is
   marked explicitly and sorts ahead of non-literal results.
2. **Lexical lane.** SQLite FTS5/BM25 over formatted task-search text.
3. **Semantic lane.** The query and corpus chunks are embedded with the same
   pinned model profile and its corpus/query instructions. Vectors are
   normalized and v1 computes exact cosine similarity.

The CLI query is plain text, not raw FTS5 query syntax. The lexical adapter
constructs and binds an escaped FTS expression from validated query terms;
quotes, operators, punctuation, and code symbols in user input cannot alter SQL
or accidentally become FTS control syntax.

The literal lane collapses to a task-level match flag. The lexical and semantic
lanes each collapse chunk results to the best rank for each task, then combine
those task ranks with reciprocal-rank fusion (RRF), using one documented
constant selected in Stage 0. RRF combines rank positions rather than
pretending BM25 and cosine scores share a calibrated numeric scale.

The final ordering is:

1. tasks with a literal match before tasks without one;
2. descending fused rank; and
3. task ID as a deterministic tie-breaker.

The result retains the winning source kind (`core` or `comment`), stable source
ID, chunk index, bounded excerpt, and the contributing lane metadata. Dense
similarity and fused scores are ranking signals, not probabilities.

There is no fixed minimum semantic-similarity threshold in v1. The command
returns the top `--limit` eligible tasks. Thresholds vary by model and corpus;
silently filtering on an uncalibrated score would hide valid results.

The literal scan is acceptable at the measured corpus size. If its measured
latency becomes material, a trigram or equivalent SQLite-native index may
replace that lane without changing the application contract.

Approximate vector indexing, reranking, multi-vector late interaction, and
learned score fusion require measured quality or latency evidence before
introduction.

### D5 — Search state lives in a task-specific SQLite sidecar

The authoritative `repo-link.db` receives only small revision-tracking metadata
described in D7. It receives no search text, FTS tables, embedding columns, or
vectors.

Store the derived task-search index in a sibling database:

```text
<authoritative-db-stem>.task-search.db
```

For the default path this is `repo-link.task-search.db`. For
`rl --db /tmp/test.db`, the sidecar is `/tmp/test.task-search.db`. No
independent path configuration is introduced in v1.

The sidecar is:

- safe to delete at any time;
- excluded from authoritative task backup, sync, snapshots, and migrations;
- rebuilt from current authoritative task content;
- limited to one active embedding profile after a successful transition;
- permitted one temporary sidecar and model profile during an atomic rebuild or
  upgrade; and
- never consulted by task mutation or GitHub reconciliation.

Search unavailability, FTS failure, model failure, or sidecar corruption must
not prevent task creation, editing, comment sync, daemon polling, or shutdown.

### D6 — Store content-addressed task-search chunks once

The sidecar has four logical storage components:

```text
search_meta(
    singleton PRIMARY KEY,
    schema_version,
    embedding_profile_id,
    chunk_format_version,
    last_applied_revision,
    built_at,
    last_reconciled_at
)

search_chunks(
    id INTEGER PRIMARY KEY,
    content_hash BLOB UNIQUE,
    text TEXT NOT NULL,
    vector BLOB NOT NULL
)

task_search_chunks(
    task_id,
    source_kind,
    source_id,
    chunk_index,
    search_chunk_id REFERENCES search_chunks(id),
    PRIMARY KEY(task_id, source_kind, source_id, chunk_index)
)

search_chunks_fts  -- FTS5 external-content index over search_chunks.text
```

`search_chunks` contains only formatted current task title/body/comment chunks.
It is an internal search projection, not a user-visible document store and not
a foundation for indexing other artifact types.

`content_hash` is the SHA-256 digest of the exact formatted UTF-8 chunk.
Identical formatted chunks share text, vector, and one FTS entry; mapping rows
connect that content to every task/source occurrence.

The core source ID is the stable literal `core`. Comment source IDs are the
stable local surrogates defined in D3.

Vectors are fixed-length little-endian float32 BLOBs. The adapter rejects a
vector with the wrong dimension or a non-finite component. Exact semantic
search streams vectors and keeps bounded task candidates; it does not load the
complete vector corpus into memory.

FTS5 uses `search_chunks` as its external-content table, avoiding a second
stored copy of formatted text. The adapter maintains both in the same sidecar
transaction. Deleting an unreferenced chunk removes its vector, formatted text,
and FTS entry together.

Result rendering verifies the current authoritative task/source and produces a
bounded excerpt. A source deleted between ranking and verification is discarded;
the command may return fewer than `--limit` results for that race, and the next
revision reconciliation repairs the sidecar. A task edited after the
reconciliation snapshot may return one-search-old evidence and is repaired by
the next revision; the sidecar never overwrites authoritative text.

### D7 — Track relevant content changes transactionally

Correct incremental reconciliation requires authoritative, monotonic change
tracking. Timestamps are ordinary task metadata and are not used as a
correctness watermark.

Add two small authoritative tables:

```text
task_search_projection_state(
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    revision INTEGER NOT NULL,
    compacted_through INTEGER NOT NULL
)

task_search_dirty_tasks(
    task_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL
)
```

`task_search_dirty_tasks.task_id` deliberately has no foreign key. A tombstone
must survive task deletion long enough for the sidecar to remove that task's
mappings.

SQLite triggers run inside the same transaction as the source mutation. Each
relevant mutation increments the singleton revision and upserts the affected
task ID with the new revision:

- task insert or delete;
- task title or body change;
- comment insert or delete;
- comment body change.

Changes to lifecycle, workspace, logical repo, filing repo, sync state,
priority, assignees, and other non-indexed metadata do not dirty embeddings.
Those fields are read from current authoritative state when computing eligible
task IDs and rendering results.

The dirty table coalesces repeated mutations to one row per task. A task is the
correct invalidation unit because a title change affects its core chunks and
every title-anchored comment. Reconciliation may re-hash unchanged chunks for a
dirty task, but content addressing embeds only genuinely new formatted
content.

Comment persistence must use D3's stable upsert semantics before these triggers
ship. Otherwise a no-op remote refresh would delete and insert every comment,
creating false revisions and needless reconciliation.

### D8 — Reconcile one global index from revision deltas

The sidecar stores `last_applied_revision = L`. A warm search reconciles as
follows:

1. read the source and sidecar revisions; if they match, skip maintenance;
2. when they differ, acquire the task-search interprocess maintenance lock;
3. re-read sidecar metadata and validate schema, format, and model profile;
4. begin a read transaction on the authoritative database;
5. read current source revision `R` and `compacted_through`;
6. if `L < compacted_through`, refuse warm reconciliation and require an
   explicit rebuild because required deltas were compacted;
7. read dirty task IDs with `L < revision <= R`;
8. read the current task title/body/comments for those IDs from the same SQLite
   snapshot; a missing task is a deletion tombstone;
9. commit the authoritative read transaction;
10. construct D2/D3 chunks and content hashes;
11. estimate resulting storage and refuse before inference if the configured
    budget would be exceeded;
12. embed only hashes absent from `search_chunks`;
13. in one sidecar transaction, replace mappings for dirty tasks, delete
    mappings for tombstoned tasks, store new chunks, prune unreferenced chunks,
    update FTS5, and set `last_applied_revision = R`; and
14. after the sidecar commit succeeds, delete authoritative dirty rows through
    `R` and advance `compacted_through` in one authoritative transaction.

The cross-database commit order is intentionally replay-safe:

- a failure before the sidecar commit leaves dirty rows available;
- a failure after the sidecar commit but before compaction leaves redundant
  dirty rows that `last_applied_revision` safely ignores; and
- a missing or older sidecar below `compacted_through` cannot guess—it requires
  a full rebuild.

The interprocess lock serializes reconcile, rebuild, clear, and model-profile
transitions. A search whose source and sidecar revisions already match does not
take the maintenance path. SQLite WAL continues to protect authoritative
read/write concurrency.

Reconciliation is always global. `--workspace`, `--repo`, and `--status` do not
limit D8 source reads or pruning; they are applied only to eligible task IDs in
D4.

A task edited after the authoritative snapshot may appear with its previous
search content for one query. Its higher revision remains dirty and is applied
by the next reconciliation. No source change can be skipped indefinitely.

### D9 — Pin a complete local embedding profile

V1 sends no title, body, comment, or query text to a remote embedding API.
After explicit model preparation, inference performs no network access.

The embedding profile is a canonical manifest containing:

- model repository and immutable revision;
- expected digest for every downloaded artifact;
- tokenizer and pooling configuration;
- corpus/query instruction prefixes;
- normalization rule;
- output dimensions;
- maximum model input;
- chunk-format version; and
- runtime-relevant options that change embedding output.

`embedding_profile_id` is the SHA-256 digest of that canonical manifest. A
mutable model name alone is not an identity.

`prepare-model` downloads into an rl-owned temporary directory, verifies the
manifest and artifact digests, and atomically installs the profile into the
rl-owned model cache. Search and rebuild open only verified local files and
must not call a runtime constructor that downloads on cache miss.

The selected profile must:

- have a license compatible with repo-link distribution;
- support local CPU inference on macOS and Linux;
- produce at most 384 dimensions, or support validated truncation to at most
  384; v1 targets 384 unless Stage 0 shows equal quality and better latency at
  256;
- document its corpus/query instructions;
- expose reliable tokenizer accounting for D2/D3; and
- pass the quality, latency, binary-size, model-size, and storage gates in §10.

The RFC deliberately does not select the model or inference runtime before the
spike. Model choice determines binary dependencies, cache size, chunk limits,
latency, and retrieval quality.

A schema, format, or embedding-profile mismatch makes `task search` refuse with
guidance to run `search-index rebuild`. Ordinary search never deletes or
rebuilds an incompatible sidecar implicitly.

### D10 — Search orchestration belongs in `application-search`

`application-query` is intentionally read-only, while task search reconciles a
derived index, compacts revision metadata, prepares models, and rebuilds
storage. Those responsibilities belong in a new `application-search` crate.

Infrastructure supplies three capabilities:

```text
TaskSearchSourceRepository
  current_revision()
  snapshot_delta(after_revision)
  full_snapshot()
  acknowledge_through(revision)
  eligible_task_ids(scope)
  current_match_source(task_id, source_id)

EmbeddingProvider
  profile_id()
  dimensions()
  input_limit()
  embed_chunks(texts)
  embed_query(query)

TaskSearchIndex
  metadata()
  missing_hashes(...)
  reconcile_tasks(...)
  search_literal(...)
  search_lexical(...)
  search_semantic_exact(...)
  clear()
  stats()
```

`application-search` owns chunk construction, revision policy, storage
preflight, hybrid fusion, task-level roll-up, and result DTOs.
`infra-sqlite` implements the authoritative source/change port and the
task-search sidecar. The selected local runtime lives in a small infrastructure
adapter. `testing-fixtures` supplies deterministic fake implementations.

No search type enters `domain-task`. Chunks, embeddings, model profiles, ranks,
and scores are derived query concerns, not task invariants.

The CLI constructs the embedding runtime and sidecar adapters only for `task
search` and `task search-index` commands. Ordinary `rl` commands do not load a
model, open the sidecar, or acquire its maintenance lock.

### D11 — CLI, JSON, rebuild, and lifecycle contract

The user-facing query is:

```text
rl task search <query> [--workspace <id>] [--repo <handle>]
                       [--status open|closed|all] [--limit <N>]
```

Search spans all workspaces by default. `--workspace` and `--repo` use existing
resolvers and narrow eligible task IDs; `--status` defaults to `all`; `--limit`
defaults to 10. `--limit 0` and an empty or whitespace-only query are rejected.

Each JSON result contains:

```json
{
  "rank": 1,
  "id": "rpl-abc",
  "task_id": "<uuid>",
  "workspace_id": "<uuid>",
  "workspace_name": "...",
  "title": "...",
  "match": {
    "literal": true,
    "lexical_rank": 3,
    "semantic_rank": 1,
    "semantic_score": 0.82,
    "fused_score": 0.03
  },
  "matched_source": {
    "kind": "core|comment",
    "source_id": "<stable local source id>",
    "remote_comment_id": null,
    "chunk_index": 0,
    "excerpt": "..."
  }
}
```

Optional lane fields are omitted when that lane did not contribute.
`semantic_score` and `fused_score` are ranking signals, not calibrated
probabilities. They are comparable only within results produced by the same
profile/index version.

Index maintenance is explicit and JSON-emitting:

```text
rl task search-index prepare-model
rl task search-index status
rl task search-index rebuild
rl task search-index clear
```

`prepare-model` is the only command permitted to download. It reports the
profile ID, pinned revision, dimensions, model size, temporary bytes required,
and final cache path.

`rebuild`:

1. verifies the prepared model profile;
2. reads a consistent full current-task snapshot and revision;
3. preflights active sidecar, temporary sidecar, active model, and staging-model
   storage;
4. builds `<db>.task-search.db.tmp` without touching the active sidecar, then
   checkpoints and closes it so no required WAL file remains;
5. reopens the temporary file and validates schema, row counts, vector
   dimensions, FTS availability, profile, and source revision;
6. syncs the validated file, atomically renames it over the active sidecar, and
   syncs the parent directory; and
7. only after a successful swap, prunes obsolete rl-owned model artifacts and
   acknowledges the built-through source revision.

A failed rebuild leaves the prior sidecar and model usable. The one-active-model
invariant applies after a successful transition; one staging profile is allowed
while preparing an atomic upgrade.

`status` reports:

- source revision, applied revision, compacted-through revision, and lag;
- dirty-task count;
- profile identity, dimensions, and format version;
- task/chunk/unique-content counts;
- raw vector, formatted-text, FTS, mapping, and total sidecar bytes;
- model-cache and staging bytes;
- bytes required for an atomic rebuild;
- configured warn/refuse budgets; and
- last successful build and reconciliation times.

`clear` removes only the disposable sidecar and temporary search files. It does
not delete tasks, comments, revision state, or the prepared model. The next
search refuses with rebuild guidance.

## 4. Crate map

```text
app-cli
  └─ rl task search / search-index
       └─ application-search: TaskSearchService
            ├─ ports::TaskSearchSourceRepository
            │    └─ infra-sqlite: repo-link.db revision + task sources
            ├─ ports::EmbeddingProvider
            │    └─ infra embedding adapter: pinned local model profile
            └─ ports::TaskSearchIndex
                 └─ infra-sqlite: repo-link.task-search.db
```

DTOs crossing into CLI JSON live in `dto-shared`, following the existing flat
query-row convention. Application-internal search types remain in
`application-search`. The CLI remains a thin composition and dispatch layer.

## 5. Storage and lifecycle invariants

1. **Task-only corpus.** Only current task title/body/comment text is indexed.
2. **Small authoritative footprint.** The authoritative DB stores only one
   revision row, coalesced dirty task IDs, comment update timestamps, and
   triggers—not vectors, FTS, or search text.
3. **One active profile.** A profile change atomically replaces the sidecar and
   eventually removes the old rl-owned model; one staging copy is allowed
   during the transition.
4. **One row per unique current chunk.** SHA-256 content addressing deduplicates
   formatted text and vectors.
5. **No history.** Snapshots and prior chunk versions are never indexed.
6. **Bounded text copy.** The sidecar stores one formatted text copy per unique
   current chunk solely for literal/FTS search and explanation.
7. **No ANN copy.** V1 stores raw vectors only; there is no HNSW/PQ/IVF copy.
8. **Atomic rebuild.** The active index survives failure; preflight budgets both
   active and staging copies.
9. **Reclaimable.** Pruning removes unreferenced current chunks; `clear` removes
   all sidecar bytes.
10. **Observable and capped.** Status exposes every storage class and the
    production path refuses before exceeding configured budgets.

At 384 float32 dimensions:

| Unique chunks | Raw vector payload |
|---:|---:|
| 1,150 measured conservative current proxy | 1.68 MiB |
| 10,000 | 14.6 MiB |
| 100,000 | 146 MiB |
| 1,000,000 | 1.43 GiB |

The measured complete hybrid sidecar used approximately 4.3 KiB per
conservative current chunk. A linear projection is roughly 42 MiB at 10,000
similar chunks and 420 MiB at 100,000, but text length, tokenizer boundaries,
deduplication, and FTS amplification vary. Stage 0 measures the real profile and
sets numeric warn/refuse thresholds; projections never replace the preflight.

The model cache is reported and budgeted separately from SQLite. It is expected
to dominate at current corpus size.

## 6. Non-goals

- Managing or indexing repository files, ADR/RFC contents, pull requests,
  commits, source code, audit logs, task snapshots, or remote issues that are
  not tasks.
- Inferring meaning absent from all current indexed task title/body/comment
  text.
- A remote embedding API or hosted vector database.
- A standalone vector-store process.
- ANN indexes, product quantization, HNSW, reranking, or GPU inference in v1.
- Cross-user or server synchronization of the task-search sidecar.
- Persisting embeddings or FTS data in task JSON, snapshots, GitHub Issues, or
  events.
- Automatically relating, editing, closing, or deduplicating tasks based on
  similarity.
- Turning `search_chunks` into a general-purpose document abstraction.

## 7. Alternatives considered

### Dense semantic search only

Rejected for the generic `task search` command. Dense retrieval bridges
paraphrases but can miss exact identifiers, code symbols, or error strings.
Literal and FTS5 lanes provide predictable exact/lexical behavior while dense
retrieval covers the semantic gap.

### FTS5 only

Rejected as the complete solution. FTS5 is compact and useful for exact and
lexical matching, but it cannot intentionally bridge relevant wording with
little token overlap. It remains one lane in D4.

### Count and `max(updated_at)` fingerprint

Rejected. Comments historically had no update timestamp, timestamps can
collide, scoped fingerprints cannot safely describe one global sidecar, and a
heuristic can leave content stale indefinitely. D7 uses transactional monotonic
revisions.

### Full corpus diff before every search

Correct but rejected as the steady-state design. It is cheap at the current
corpus, but work grows with all tasks rather than changed tasks. The coalesced
dirty-task table keeps warm reconciliation proportional to the delta while a
full scan remains the explicit rebuild path.

### Append-only search change log

Rejected in favor of a coalesced dirty-task table. Search only needs the latest
current state for each affected task, not every intermediate mutation. One row
per dirty task bounds unattended growth and naturally collapses edit bursts.

### Per-comment invalidation

Rejected. Every comment chunk includes the task title, so a title change would
still need to fan out across all comments. Task-level invalidation is simpler
and correct; content hashes avoid re-embedding unchanged chunks.

### Semantic orchestration in `application-query`

Rejected because search reconciliation, compaction, model preparation, and
rebuild mutate derived state. `application-search` preserves
`application-query`'s read-only contract.

### Embedding or FTS columns in `repo-link.db`

Rejected. Search text, vectors, and FTS are large, profile-specific,
replaceable data and do not belong in the authoritative backup lifecycle.
Only tiny revision metadata belongs in the source database.

### LanceDB or another vector store

Rejected for v1. Current scale does not require a vector-store dependency,
index-version retention, compaction lifecycle, or separate process. SQLite
plus exact cosine and FTS5 is sufficient and inspectable.

### Delete the old sidecar before rebuilding

Rejected. It saves temporary disk but turns an inference, disk, or validation
failure into avoidable search downtime. D11 preflights and atomically swaps a
temporary sidecar.

### Eager embedding on every task/comment mutation

Rejected. Ordinary task correctness must not depend on model availability, and
a local edit must not wait for CPU inference. Authoritative triggers record only
small revision metadata; search performs inference later.

### Daemon-maintained search index

Rejected for v1. It adds another background lifecycle and recovery path before
search frequency requires one. The revision feed permits a future daemon
consumer without changing task mutation semantics.

### Remote embedding API

Rejected. Sending task/comment content externally conflicts with the local-first
default and makes offline search impossible.

## 8. Risks and mitigations

- **Model quality.** General embeddings may collapse repo-specific distinctions.
  Mitigation: the labelled evaluation and predeclared acceptance gate in §10.
- **Exact identifiers.** Dense models may tokenize opaque strings poorly.
  Mitigation: literal and FTS5 lanes rank exact text independently of embeddings.
- **Language coverage.** An English-only model may fail on multilingual tasks.
  Mitigation: represent actual supported task languages in the evaluation
  fixture and select the profile accordingly.
- **Long-content truncation.** A runtime may truncate silently.
  Mitigation: tokenizer-owned limits, no-discard chunking, and oversized
  title/paragraph tests.
- **Chunk explosion.** Small limits or overlap can multiply vectors.
  Mitigation: no overlap, preflight chunk counts, content hashing, and a hard
  storage budget.
- **Long/noisy task bias.** A task with many chunks has more chances for a high
  dense score.
  Mitigation: task-level per-lane collapse plus explicit noisy-thread cases in
  the evaluation set.
- **Missed invalidation.** A new write path might omit application-level
  bookkeeping.
  Mitigation: SQLite triggers observe storage mutations regardless of caller,
  and migration tests enumerate every relevant column/action.
- **Remote comment churn.** Wholesale refresh would dirty every task repeatedly.
  Mitigation: D3 stable upserts and body-aware no-op updates.
- **Cross-database crash.** Source metadata and sidecar cannot share one atomic
  commit.
  Mitigation: replay-safe sidecar-first commit order and compacted-gap detection.
- **Concurrent maintenance.** Two CLI processes could reconcile or rebuild
  against different revisions.
  Mitigation: one task-search interprocess lock plus metadata revalidation after
  lock acquisition.
- **Rebuild disk pressure.** Atomic replacement temporarily needs two indexes
  and may need two models.
  Mitigation: preflight the actual active/staging requirement before download,
  inference, or file growth.
- **SQLite FTS availability.** A platform SQLite build might lack FTS5.
  Mitigation: verify the production-linked SQLite feature during Stage 0 and
  fail preparation/rebuild with a clear diagnostic rather than silently
  dropping the lexical lane.
- **Model supply chain.** Mutable upstream artifacts could change output or
  introduce unverified files.
  Mitigation: immutable revisions, manifest digests, rl-owned cache, and atomic
  verified installation.
- **Sensitive local text copy.** The sidecar contains formatted task text.
  Mitigation: same-directory permissions as the authoritative DB, no sync or
  backup, bounded current-only content, and explicit `clear`.
- **Misleading scores.** Users may read cosine or fused scores as confidence.
  Mitigation: expose lane metadata, call them ranking signals, and never label
  them probabilities.

## 9. Staged implementation plan

### Stage 0 — Quality, runtime, latency, and storage spike

- Build a reviewed synthetic or sanitized task-only evaluation corpus using the
  D2/D3 formatting rules.
- Record literal and FTS5 baselines before scoring embedding candidates.
- Declare quality, exact-match retention, p95 latency, binary-size, model-size,
  and storage acceptance thresholds before comparing candidates.
- Evaluate a small set of eligible local models at no more than 384 dimensions.
- Measure tokenizer-derived chunk counts, batch sizes, indexing throughput,
  warm-delta latency, exact-vector query latency, Recall@10, MRR, literal/FTS
  behavior, RRF settings, raw vector bytes, complete sidecar bytes, temporary
  rebuild bytes, and model-cache bytes.
- Select the model, immutable revision, runtime, tokenizer limits, batch size,
  RRF constant, and numeric warn/refuse budgets.
- Verify CPU support and linked FTS5 availability on supported macOS and Linux
  builds.

No production search schema or CLI contract ships before this evidence passes
the declared gate.

### Stage 1 — Authoritative revision feed and stable comment identity

- Add `task_comments.updated_at` and backfill it.
- Replace wholesale synced-comment persistence with stable upsert/delete.
- Preserve a pending comment's local surrogate through remote promotion.
- Add `task_search_projection_state`, `task_search_dirty_tasks`, and triggers.
- Add migration, adapter, crash-order, and no-op-refresh tests.

### Stage 2 — Search application and deterministic fixtures

- Add `application-search`.
- Add the three D10 ports and result DTOs.
- Implement deterministic fake source, embedding, and search-index adapters.
- Implement D2/D3 formatting, versioned chunking, SHA-256 hashing, global
  revision reconciliation, storage preflight, task-level lane roll-up, RRF, and
  explanation selection.

### Stage 3 — SQLite sidecar and pinned local embedder

- Add sidecar schema, FTS5 maintenance, literal search, lexical search, exact
  cosine scan, pruning, stats, interprocess locking, and atomic rebuild.
- Add the selected local embedding adapter and verified rl-owned model cache.
- Implement profile mismatch, compacted-gap, corruption, and disk-budget error
  paths.

### Stage 4 — CLI and operational surface

- Add `rl task search` and `rl task search-index`.
- Wire cwd-aware workspace/repo resolution and current lifecycle filtering.
- Add prepare-model, rebuild, clear, status, progress on stderr, and JSON on
  stdout.
- Refresh `rl agents docs` for the new command help.

### Stage 5 — Measure before extending

- Run the labelled fixture against the finished production path.
- Record p50/p95 warm reconciliation, exact/lexical/semantic query latency,
  rebuild time, model memory, sidecar amplification, and temporary peak disk.
- Add ANN, quantization, reranking, background maintenance, or overlap only
  through a follow-up RFC backed by observed need.

## 10. Testing and evaluation strategy

### Deterministic unit and integration tests

- Short title/body content produces one core chunk.
- Empty body produces title-only content.
- Every long-body/comment chunk preserves all source text without overlap.
- An oversized paragraph falls back from paragraph to sentence/token splitting.
- An oversized title remains fully searchable and body chunks use a marked,
  deterministic bounded anchor.
- Comment author/timestamp are omitted from indexed text.
- Stable remote-comment refresh preserves local source IDs and creates no dirty
  revision when content is unchanged.
- A remote comment body edit with unchanged creation time increments revision
  and replaces only affected search content.
- Pending-comment promotion preserves its local search source ID and does not
  dirty unchanged indexed text.
- Task title edits invalidate core and all title-anchored comments.
- Task body/comment edits dirty the task, while lifecycle/repo/status-only edits
  do not dirty embeddings.
- Task deletion leaves a tombstone until its sidecar mappings are removed.
- Repeated task mutations coalesce to one dirty row at the latest revision.
- Identical formatted chunks share stored text/vector/FTS content.
- Reconciliation is idempotent when source revision is unchanged.
- A body edit replaces mappings and prunes orphaned chunks.
- Snapshots never create search chunks or revision events.
- Search filtering reads current workspace/repo/lifecycle state without
  reconciling a scoped index.
- Literal matches sort ahead of non-literal results.
- Quotes, operators, punctuation, and code symbols are treated as plain query
  input and cannot alter SQL or FTS syntax.
- FTS and semantic lane ranks collapse to task level and fuse deterministically.
- Ties resolve by task ID.
- A cold sidecar refuses with rebuild/prepare guidance.
- Schema, format, or profile mismatch refuses rather than rebuilding inline.
- `last_applied_revision < compacted_through` requires a full rebuild.
- Failure before sidecar commit leaves dirty rows replayable.
- Failure after sidecar commit but before acknowledgement is idempotent.
- Concurrent reconcile/rebuild/clear operations serialize through the
  interprocess lock and cannot regress the applied revision.
- Reconciliation updates mappings, FTS, pruning, and metadata atomically.
- A source deleted between ranking and result verification is omitted safely.
- A failed temporary rebuild preserves the active sidecar.
- A successful rebuild atomically swaps and then prunes obsolete model state.
- Search and rebuild perform no network access when the model is absent.
- Storage preflight refuses before model inference or sidecar growth.
- Search/index failures never mutate or block authoritative task operations.
- `status`, `rebuild`, `clear`, and `prepare-model` emit JSON and affect only
  their documented search/model/revision state.

### Retrieval-quality evaluation

The checked-in evaluation fixture must be synthetic or explicitly reviewed and
sanitized. Raw local task/comment content must never be committed merely because
it was used during the spike.

The fixture contains task content, queries, and labelled relevant task IDs. It
must include:

- exact identifiers, phrases, code symbols, and error strings;
- paraphrases with little or no token overlap;
- clusters containing design, implementation, bug, and follow-up language;
- opaque identifiers with and without surrounding semantic context;
- misleading generic software terms;
- long descriptions and comments whose relevant paragraph is not first;
- tasks with many irrelevant comments/chunks to expose length bias;
- supported task languages; and
- closed historical tasks.

Report literal, FTS-only, semantic-only, and fused results. For each candidate
profile/dimension, report at least Recall@10 and MRR, plus exact-match retention,
false positives, and false negatives. Candidate selection must follow
predeclared thresholds; a model is not accepted merely because it is the best
of a weak set.

Real-model tests are opt-in and use only explicitly prepared local profiles.
Normal CI downloads no model and uses deterministic fakes so builds remain
offline and reproducible.

### Performance and storage evaluation

Measure:

- cold full rebuild time and peak resident memory;
- warm reconciliation for zero, one, and bursty dirty-task deltas;
- literal, FTS, exact-vector, and fused p50/p95 query latency;
- authoritative revision metadata growth;
- raw vectors, formatted text, FTS, mappings, total sidecar, and model cache;
- atomic rebuild peak disk requirement; and
- behavior at measured current scale plus synthetic 10k and 100k chunk scales.

The numeric production budgets are recorded in the Stage 0 report and then
folded into this RFC before implementation is accepted.

## 11. Open questions

### Resolved

- **Corpus:** current task title/body/comments only; no general document
  management or repository-file indexing.
- **Retrieval:** literal + FTS5/BM25 + exact dense cosine, fused at task level.
- **Freshness:** transactional monotonic revision plus coalesced dirty task IDs;
  no timestamp/count fingerprint.
- **Scope:** one global index; workspace/repo/lifecycle filters are query-time
  eligibility only.
- **Comment identity:** stable local surrogate, remote ID as metadata, remote
  updates upserted in place.
- **Rebuild:** preflighted temporary sidecar and atomic swap; failed rebuilds
  preserve the active index.
- **Application boundary:** stateful orchestration lives in
  `application-search`, not `application-query`.
- **Dimensions:** target 384; use 256 only if Stage 0 shows equal quality and
  better latency.
- **Installation:** explicit verified `prepare-model` is the only download path.
- **Default result count:** 10.
- **Pending comments:** searchable immediately.
- **Storage direction:** hard preflight covers authoritative metadata, active
  and staging sidecars, and active/staging model cache.

### Remaining, Stage 0 gated

1. Which local model, immutable revision, and Rust inference runtime pass D9 and
   the §10 gate?
2. What tokenizer-derived input limit and batch size fit supported machines?
3. What RRF constant and FTS tokenizer/query settings produce the best labelled
   task-level results?
4. What exact quality and latency acceptance thresholds are required before
   model comparison?
5. What numeric warn/refuse budgets apply to sidecar size, model cache, and
   atomic rebuild peak disk?

## 12. References

- SQLite Vec1 recommends exact nearest-neighbour mode for relatively small
  vector sets (approximately fewer than 5,000) and requires trained ANN models
  at larger scale: <https://sqlite.org/vec1/doc/trunk/doc/vec1intro.md>.
- SQLite FTS5 provides BM25 lexical ranking and the virtual-table primitives
  used by the lexical lane: <https://sqlite.org/fts5.html>.
- Sentence Transformers recommends distinct query/corpus encoding for
  asymmetric semantic search and documents exact, chunked corpus scans for
  small corpora:
  <https://www.sbert.net/examples/sentence_transformer/applications/semantic-search/README.html>.
- Reciprocal rank fusion combines independent result rankings without requiring
  score calibration: <https://doi.org/10.1145/1571941.1572114>.
- LanceDB documents version retention and delayed disk reclamation, illustrating
  lifecycle avoided by the single disposable SQLite sidecar:
  <https://docs.lancedb.com/tables/versioning> and
  <https://docs.lancedb.com/indexing/reindexing>.
