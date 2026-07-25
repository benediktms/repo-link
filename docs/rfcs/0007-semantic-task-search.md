# RFC 0007 — Local task search over current task content

Status: Draft (2026-07-21, revised 2026-07-25: full-hash reconciliation with
an in-place transactional sidecar)
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

This RFC uses the term **search chunk** for one formatted piece of task
content. It deliberately does not use a generic `documents` table or API that
could imply a broader document-management feature.

### Current persistence shape

- `tasks.title` and `tasks.body` hold the current task summary and description.
- `task_comments` stores current synced and pending comments on a separate
  persistence axis; comment writes do not append task snapshots or change
  `sync_state`.
- Synced comments are currently replaced wholesale on refresh, which changes
  their local surrogate IDs even when their remote identity and content are
  unchanged. This design absorbs that churn instead of requiring a persistence
  rewrite (D3, D6).
- `task_snapshots` contains append-only history and repeats title/body over
  time. Snapshot history is audit data, not search input.
- SQLite is authoritative for task state. The search index is a derived
  projection that cannot affect task correctness or sync.

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

A conservative prototype split the current corpus into 1,150 chunks of at most
1,200 text bytes, stored 384-dimensional float32 vectors, retained the
formatted search text, and built FTS5. The complete index was 4.79 MiB after
`VACUUM` — roughly a quarter of the authoritative database. Raw vectors alone
were 1.68 MiB.

Two consequences drive the design:

1. **Reading and hashing the whole corpus is milliseconds.** 0.91 MiB of
   current text can be re-read, re-chunked, and re-hashed on every search.
   Only embedding is expensive, and content addressing limits embedding to
   genuinely new text regardless of how change is detected. Incremental
   change tracking is not justified before the Stage 0 measurement crosses
   the D6 ceiling (§7).
2. **The embedding model dominates disk**, not the index. Storage becomes a
   problem through retained model versions or unbounded chunking — not through
   current task volume.

## 2. Goals

1. Search current task title, body, and comments using exact, lexical, and
   semantic retrieval.
2. Keep title and body together so the title anchors the description's meaning.
3. Preserve all source text in literal, lexical, and semantic retrieval; no
   lane silently truncates indexed content.
4. Re-derive the index shape before every query. Concurrent source edits may be
   one query behind, but stale evidence is never returned and the next search
   converges without relying on a write-path change feed.
5. Keep model inference local and prevent ordinary search from downloading.
6. Search is usable without any model: the literal and lexical lanes degrade
   gracefully instead of refusing.
7. Keep the search index disposable, inspectable, rebuildable, size-capped,
   and isolated from the authoritative database and task correctness.
8. Maintain one global current-task index; apply workspace, repo, and lifecycle
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

Search maintains one global current-task index. A scoped query never builds or
prunes a scoped sub-index.

### D2 — Title and body form the core search content; chunking is deterministic and model-independent

For a short task, construct one core search chunk with stable field labels:

```text
Title: <task.title>

Description:
<task.body>
```

Title and body are not embedded independently. The title supplies the task's
subject; the body supplies detail. Averaging or ranking independent title/body
vectors would discard that relationship.

Lexical chunk boundaries are **model-independent**: the chunker packs complete
body paragraphs under a fixed UTF-8 byte budget (initially approximately 900
bytes of body text per chunk, recorded in `chunk_format_version`), splits an
oversized paragraph at sentence boundaries and then valid UTF-8 scalar
boundaries, and prepends the complete title to every body chunk. When the title
itself exceeds the budget, body chunks carry a deterministic, explicitly
`…`-marked title anchor and the full title is still indexed as its own chunk.
An empty body produces title-only core content. No source text is dropped.

When a model profile is active, each lexical chunk is further split into one or
more **semantic inputs** using that profile's tokenizer. The effective token
budget includes instruction prefixes, field labels, and model special tokens.
Every lexical chunk byte is covered by at least one semantic input; the adapter
must never rely on the model runtime's truncation behavior. Semantic inputs map
back to their lexical chunk and may change when the embedding profile changes
without rebuilding FTS or formatted text.

V1 uses no overlapping chunks. Paragraph-preserving boundaries plus title
anchoring retain context without duplicating an arbitrary overlap window.

Chunk formatting and lexical boundary rules carry an explicit
`chunk_format_version` in index metadata. Tokenizer-specific semantic-input
rules are part of the embedding profile. D8 defines version transitions.

### D3 — Comments are separate, title-anchored search content

Each comment is formatted separately:

```text
Title: <task.title>

Comment:
<comment.body>
```

Author and timestamp are omitted from the indexed text because they are
metadata, not task meaning. A long comment follows D2's chunking rule.

The implementation must not concatenate the entire comment thread into core
content. Threads can be long, repetitive, or operational; merging them would
dilute the task's primary meaning and make every comment change re-embed the
whole thread.

**Comment identity is deliberately not a search dependency.** Search chunks
are keyed by `(task_id, content_hash)` (D5), so the wholesale delete-and-
reinsert behaviour of current comment persistence — which churns local
surrogate IDs on every refresh — produces byte-identical formatted chunks and
therefore zero index writes and zero re-embeds. The stable comment-upsert
rework (adding `task_comments.updated_at`, upserting by
`(task_id, remote_comment_id)`) remains desirable for its own reasons but is
**not a prerequisite** of this RFC.

Result evidence still names the source comment where possible: when the
winning chunk has comment kind, result rendering joins the chunk text back to
the current `task_comments` rows of that task and reports
`remote_comment_id` when the match is unambiguous (D10).

### D4 — Hybrid exact, lexical, and semantic retrieval with query modes

`rl task search` is a general task-search command, not a dense-vector
debugging surface. V1 combines three retrieval lanes:

1. **Literal lane.** Case-folded (Unicode) substring matching **over raw
   authoritative text**: `tasks.title`, `tasks.body`, and
   `task_comments.body`. Running this lane over the authoritative rows rather
   than formatted chunks means a literal query can never be broken by a chunk
   boundary and can never false-match the injected `Title:` /
   `Description:` / `Comment:` labels. The scan covers ~0.91 MiB and needs no
   index, no model, and no reconciliation to be fresh. The needle is the full
   query string; in identifier mode the lane additionally tests each
   identifier-shaped token as its own needle. If its measured latency
   ever becomes material, the named replacement is an FTS5 trigram-tokenizer
   index (already compiled into the bundled SQLite) whose candidates are
   verified by the same substring test.
2. **Lexical lane.** SQLite FTS5/BM25 over formatted chunk text, tokenizer
   `unicode61` with `tokenchars '_-'` so code identifiers survive
   tokenization. The adapter builds the MATCH expression from the plain-text
   query only: each term is double-quoted with internal double quotes doubled,
   terms are joined with ` OR `, and the resulting string is bound as a
   parameter. No user character can become FTS syntax or SQL.
3. **Semantic lane.** The query and corpus chunks are embedded with the same
   pinned model profile and its corpus/query instructions. Vectors are
   normalized and v1 computes exact cosine similarity by brute-force scan over
   all available vectors. A query whose instruction-prefixed text exceeds the
   model input limit is never truncated: the semantic lane is skipped for that
   query with an explicit reason in JSON and stderr, while literal and lexical
   retrieval still use the complete query.

**Query modes.** `--exact` explicitly selects exact mode. Otherwise a small
deterministic token classifier selects identifier mode when any token has
identifier shape (contains `_`, `::`, `/`, `#` followed by digits, a `rpl-`
style prefix, mid-word capitals, two-plus uppercase letters, or a
digits-and-letters error-code shape). All other queries use natural-language
mode. Shell quoting only groups an argument and is not treated as a signal
because quote characters are removed before the CLI receives the value. The
selected mode is reported in JSON.

The lexical and semantic lanes collapse chunk results to the best rank per
task and are combined with reciprocal-rank fusion (RRF), constant `k = 60`
(the literature default; the fixture may revise it). RRF combines rank
positions rather than pretending BM25 and cosine scores share a calibrated
numeric scale. The literal lane participates by mode:

- **exact mode:** tasks containing the complete query as a substring sort ahead
  of all other tasks, then by fused rank.
- **identifier mode:** tasks containing the full query — or, failing that,
  every identifier-shaped token — as substrings sort ahead of all other
  tasks, then by fused rank. Exact lookup keeps its hard guarantee exactly
  where the user signalled exactness, including mixed identifier+prose
  queries whose full sentence never occurs verbatim.
- **natural-language mode:** the full-query literal match set forms a third
  RRF lane (ranked by occurrence count, ties by task ID) instead of a hard
  sort, so an incidental substring cannot outrank the true paraphrase answer.

Ties resolve by task ID in all modes.

Results retain the winning source kind (`core` or `comment`), a bounded excerpt
windowed around the match position, and per-lane metadata. A literal-only win
derives its kind from the matched raw column (title/body → `core`, comment body
→ `comment`) and its excerpt from the raw text; it maps to no stored chunk.
Before rendering, the command verifies the current authoritative task/source.
A source deleted or changed after reconciliation is omitted, so a racing edit
may produce fewer than `--limit` results or one-query-old recall; the next
search repairs the sidecar. Dense similarity and fused scores are ranking
signals, not probabilities.

There is no fixed minimum semantic-similarity threshold in v1. The command
returns the top `--limit` eligible tasks. Thresholds vary by model and corpus;
silently filtering on an uncalibrated score would hide valid results.

Approximate vector indexing, reranking, multi-vector late interaction, and
learned score fusion require measured quality or latency evidence before
introduction (§7 "Late interaction and reranking").

### D5 — Search state lives in an in-place SQLite sidecar

The authoritative `repo-link.db` receives no search tables, triggers, FTS data,
formatted search text, or vectors. Store the derived index beside it by
appending a suffix to the complete authoritative filename:

```text
<authoritative-db-filename>.task-search.db
```

The default is `repo-link.db.task-search.db`; `rl --db /tmp/test.db` uses
`/tmp/test.db.task-search.db`. Appending to the full filename avoids the
stem-collision in the previous draft.

The sidecar is created with `auto_vacuum=FULL` before any tables, WAL journaling,
foreign keys enabled, and a `max_page_count` derived from the hard index budget.
Unlike enabling auto-vacuum on the existing authoritative database, this needs
no one-time rewrite and affects only disposable search storage.

The sidecar schema is:

```text
task_search_meta(
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    schema_version INTEGER NOT NULL,
    chunk_format_version INTEGER NOT NULL,
    embedding_profile_id TEXT            -- NULL until a prepared profile claims it
)

task_search_chunks(
    id INTEGER PRIMARY KEY,              -- rowid, shared with FTS
    task_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('core','comment')),
    content_hash BLOB NOT NULL,          -- SHA-256 of formatted UTF-8 text
    text TEXT NOT NULL,                  -- immutable lexical chunk
    UNIQUE(task_id, content_hash)
)

task_search_vectors(
    search_chunk_id INTEGER NOT NULL
      REFERENCES task_search_chunks(id) ON DELETE CASCADE,
    segment_index INTEGER NOT NULL,
    embedding_input_hash BLOB NOT NULL,
    vector BLOB NOT NULL,                -- little-endian normalized f32
    PRIMARY KEY(search_chunk_id, segment_index)
)

task_search_fts                          -- FTS5 external-content over task_search_chunks(text)
```

Keying lexical chunks by `(task_id, content_hash)` makes reconciliation
convergent and makes comment-surrogate churn invisible (D3). Within-task
duplicate formatted chunks collapse to one row. Cross-task deduplication is
deliberately omitted at current scale. Semantic-input rows map one or more
tokenizer-bounded vectors back to each lexical chunk.

`task_search_fts` uses the chunk table as external content and follows SQLite's
documented insert/delete trigger pattern. A guard trigger rejects updates to
chunk identity or text; chunks are deleted and inserted rather than mutated.
Vector rows live separately, so embedding updates cannot desynchronize FTS.
`status` and `rebuild` run FTS5 `integrity-check`.

FTS5 and DBSTAT availability are compile-time facts in the current bundled
SQLite build. An offline CI smoke test pins both capabilities.

A stable sibling `<sidecar>.lock` file is never renamed or deleted. Every
command that opens the sidecar holds a standard-library shared file lock for
the connection lifetime, except `search-index clear`, which obtains the
exclusive lock before opening it. Normal reconcile and rebuild operations use
ordinary SQLite transactions and update the existing database in place; they
never unlink, rename, or swap it. A failed transaction leaves the previous
committed state intact.

`search-index clear` transactionally deletes all derived rows, commits the full
auto-vacuum, and runs `PRAGMA wal_checkpoint(TRUNCATE)` before releasing its
exclusive lock. If the sidecar cannot be opened as SQLite, the command instead
confirms the failure while exclusively locked, removes the sidecar together
with its `-wal`/`-shm`, and creates a fresh sidecar. Cooperating processes
cannot retain an old database handle or mispair WAL state. Ordinary search
never performs this destructive recovery.

Before adding text, FTS entries, vectors, or model artifacts, the command
preflights projected bytes and filesystem headroom. `max_page_count` is the
native final backstop for the sidecar; an exceeded limit rolls back the derived
write and never affects authoritative task operations. The model cache has a
separate hard budget and refuses a new prepared profile until enough space is
available or an explicitly selected old profile is removed. Numeric sidecar/WAL
thresholds are selected and folded into this RFC before Stage 1 ships; the
model-cache threshold is fixed before Stage 3.

Full auto-vacuum moves reclaimable pages to the end at clear's commit; the
truncating checkpoint then materializes the smaller database and removes
reclaimable WAL bytes. This shrinks disposable storage without vacuuming or
rewriting `repo-link.db`; status reports residual allocation caused by
partially filled pages.

### D6 — Freshness by serialized per-search hash-diff reconciliation

Every `rl task search` begins with a full-corpus reconcile:

1. open the sidecar and validate its schema/format state under D8;
2. start `BEGIN IMMEDIATE` on the **sidecar**, establishing the reconciliation
   session before reading authoritative content;
3. in one read transaction on `repo-link.db`, load all current task
   title/body/comment rows; never read snapshots or audit data;
4. construct deterministic D2/D3 lexical chunks and SHA-256 hashes;
5. calculate the desired `(task_id, content_hash)` set and projected storage;
   refuse before growth if D5 budgets or filesystem headroom would be exceeded;
6. inside the still-open sidecar transaction, delete rows absent from the
   desired set and insert missing rows; FTS follows through D5 triggers; and
7. commit the sidecar transaction.

The ordering is intentional. SQLite serializes sidecar writers, and every
later reconciler takes its authoritative snapshot only after acquiring that
writer slot. A slow reconcile therefore cannot land an older snapshot after a
newer reconcile. The authoritative database remains read-only throughout and
its writer slot is never held by search.

A zero-change reconcile writes no rows. Source edits committed after step 3's
snapshot may remain one query behind; result verification in D4 removes stale
evidence, and the next reconcile repairs omissions or changed chunks. No
source write can be skipped indefinitely.

When the sidecar's active profile matches a prepared local profile, semantic
input planning and inference happen after the lexical transaction commits.
Missing semantic inputs are embedded in batches. Each vector insert is one
guarded statement that succeeds only when:

- sidecar metadata still names the same embedding profile;
- `search_chunk_id`, `task_id`, and `content_hash` still identify the source
  chunk; and
- `embedding_input_hash` still matches the tokenizer-derived input.

If any guard fails, the stale batch result is discarded. This prevents both
ROWID reuse and concurrent chunk/profile transitions from attaching a vector
to the wrong text. Vector dimensions and finite components are validated
before storage. Exact semantic search streams vectors with a bounded top-k
heap.

The read/chunk/hash/diff target is under approximately 20 ms at current scale.
Incremental authoritative change tracking remains a follow-up only if measured
p95 reconciliation exceeds approximately 150 ms or 30 MiB of current source
text. Until then, O(corpus) per search is the explicit, measured ceiling.

### D7 — Pin one complete local embedding profile with an in-binary trust root

V1 sends no title, body, comment, or query text to a remote embedding API.
After explicit model preparation, inference performs no network access.

The embedding profile is a canonical manifest containing: model repository and
immutable revision; SHA-256 digest and size for every artifact; tokenizer and
pooling configuration; corpus/query instruction prefixes; normalization rule;
output dimensions; maximum model input; and deterministic semantic-input
splitting rules. `embedding_profile_id` is the SHA-256 of the canonical
manifest. Lexical chunk formatting is versioned separately in
`task_search_meta.chunk_format_version` (D2), so a lexical format bump never
changes the model-cache identity.

**The trust root is the repo-link source tree.** The manifest — including
every artifact digest — is authored during profile selection (§9 Stage 2),
checked into the repository, and compiled into the binary. `prepare-model`
downloads into an rl-owned temporary directory, verifies every byte against
the embedded digests, and atomically renames the profile into the cache. It
never trusts an upstream-fetched manifest, config, or file list. Verification
is therefore not trust-on-first-use: a swapped or tampered upstream artifact
fails at first install. Only digest-pinned artifact formats are permitted —
`safetensors` weights and JSON/text configs; pickle-based formats
(`pytorch_model.bin`) are excluded outright.

**The model cache is global and content-addressed:**
`<platform data dir>/repo-link/models/<embedding_profile_id>/`. It is keyed by
profile digest, so multiple profiles coexist and multiple databases share one
cache. `prepare-model` preflights the manifest-declared download, staging copy,
final cache size, filesystem headroom, and the hard global model-cache budget
before network or file growth. Removing a superseded profile is explicit
(`search-index prepare-model --remove <profile-id>`), never a side effect of
building an index for one particular database.

**The inference runtime is committed, not open:** candle
(`candle-core`/`candle-transformers`) + `tokenizers` + memory-mapped
`safetensors`, CPU only, in a new `infra-embed` crate. The selected dependency
features must perform no build-script network download, require no external C++
runtime, pass `cargo build --offline` after ordinary dependency fetch, and meet
the repository's `-D warnings` gates. (`ort`'s default feature downloads
onnxruntime binaries in `build.rs`; vendoring it adds cmake and a C++ toolchain.
Rejected, §7.) Candle's actual cold-load and CPU inference cost is measured in
Stage 2/4 rather than assumed. The `EmbeddingProvider` port keeps the choice
reversible: a runtime swap is an explicit profile transition, not a task-data
migration.

The selected model must: have a license compatible with repo-link
distribution; run on CPU on macOS and Linux via candle; produce at most 384
dimensions; document its corpus/query instructions; and pass the per-category
fixture gates in §10. The candidate set is small and named —
`bge-small-en-v1.5`, `e5-small-v2`, `all-MiniLM-L6-v2`, plus
`multilingual-e5-small` if the fixture demands non-English coverage — and the
winner is pinned as the single shipped profile.

### D8 — Degrade safely; make version transitions explicit

`rl task search` always returns the best results the current state supports:

- **No sidecar:** search creates one for the current schema/chunk version and
  builds literal+lexical state under D5/D6.
- **No model prepared:** literal and lexical lanes run; JSON reports
  `"semantic_available": false` and stderr names `prepare-model`. Searching for
  an identifier never requires a model download.
- **Unclaimed profile:** when metadata is NULL and the current binary's
  prepared profile exists, one compare-and-set claims it. Other binaries then
  observe that choice rather than replacing it.
- **Different embedding profile:** literal and lexical lanes remain available,
  but semantic search is skipped. Ordinary search never clears vectors,
  changes the profile ID, or re-embeds into a different space. An explicit
  `search-index rebuild` performs the transition.
- **Different sidecar schema or lexical chunk format:** search falls back to the
  raw literal lane with `"lexical_available": false`; it does not rewrite the
  sidecar. `search-index rebuild` explicitly replaces the derived contents
  in-place for the current binary.
- **Missing matching cache:** stored vectors remain untouched, but the semantic
  lane is unavailable until that exact profile is prepared again.
- **Embedding failure:** already-stored vectors for the active profile keep
  serving; missing semantic inputs remain absent until a later batch succeeds.
- **FTS corruption in an openable sidecar:** the authoritative literal lane
  still works and the command reports `search-index rebuild` guidance.
- **Unopenable sidecar:** the authoritative literal lane still works and the
  command reports `search-index clear` guidance. That explicit command uses
  D5's exclusive lifecycle lock to recreate only the disposable sidecar.

These rules prevent alternating old and new binaries from repeatedly claiming
metadata, wiping vectors, or rebuilding lexical chunks. Hard errors are
reserved for authoritative SQLite failures and explicit maintenance failures;
search-index faults never mutate or block task operations.

### D9 — Search orchestration belongs in `application-search`

`application-query` is intentionally read-only, while task search writes a
derived projection and prepares models. Those responsibilities go to a new
`application-search` crate.

Infrastructure supplies three capabilities:

```text
TaskSearchSourceRepository      (infra-sqlite: repo-link.db)
  load_snapshot()               -- current task/comment text, one read snapshot
  eligible_task_ids(scope)
  verify_match_sources(...)
  search_literal(query)

TaskSearchIndex                 (infra-sqlite: task-search sidecar)
  begin_reconcile()             -- starts sidecar BEGIN IMMEDIATE
    -> TaskSearchReconcileSession
       diff_chunks(desired, projected_bytes)
       commit()
  metadata()
  claim_empty_profile(expected_profile)
  missing_semantic_inputs(limit)
  store_vectors_guarded(...)
  search_lexical(match_expr)
  search_semantic(query_vector)
  stats()
  clear()

EmbeddingProvider               (infra-embed)
  profile_id()
  dimensions()
  input_limit()
  plan_semantic_inputs(text)     -- complete tokenizer-bounded coverage
  embed_inputs(texts)
  embed_query(query)
```

`application-search` owns chunk construction, hashing, the reconcile policy,
query-mode classification, lane roll-up, RRF fusion, excerpt selection, and
semantic-input coverage validation. Public search rows and the top-level CLI
response live in `dto-shared`, preserving the repository's cross-boundary DTO
rule; application-internal orchestration types stay in `application-search`.
`testing-fixtures` supplies a deterministic fake embedder (hash-derived
pseudo-vectors) so CI stays offline.

No search type enters `domain-task`. The CLI constructs the embedding runtime
and sidecar adapter only for `task search` and `task search-index`; ordinary
commands load neither. There is no cargo feature in v1. If measured binary size
ever demands a slim build, a `search` feature ships only with a dedicated
`cargo check --no-default-features` CI leg.

### D10 — CLI, JSON, and lifecycle contract

The user-facing query is:

```text
rl task search <query> [--workspace <id>] [--repo <handle>]
                       [--status open|closed|all] [--limit <N>] [--exact]
```

Search spans all workspaces by default: an **omitted** `--workspace`/`--repo`
means **no filter** — the cwd-derivation path is deliberately not invoked.
An explicit `--repo` resolves through the existing repo-handle resolver
(prefix/name/alias, keeping its ambiguity/exit-2 contract); an explicit
`--workspace` remains a verbatim workspace UUID, as in every existing
command.
`--status` defaults to `all`; `--limit` defaults to 10. `--limit 0` and an
empty or whitespace-only query are rejected.

Top-level JSON includes the selected mode, lane availability, and results:

```json
{
  "query": "retry-safe event",
  "query_mode": "natural",
  "lexical_available": true,
  "semantic_available": true,
  "results": [
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
        "kind": "core",
        "remote_comment_id": null,
        "excerpt": "..."
      }
    }
  ]
}
```

Optional lane fields are omitted when that lane did not contribute.
`semantic_skipped_reason` is omitted when semantic search was available and
the query fit the profile.
`semantic_score` and `fused_score` are ranking signals, not calibrated
probabilities, comparable only within one profile/index version.
`remote_comment_id` is present when the winning chunk is a comment whose
current row is unambiguously identified (D3).

Index maintenance is explicit and JSON-emitting:

```text
rl task search-index prepare-model [--remove <profile-id>]
rl task search-index status
rl task search-index rebuild
rl task search-index clear
```

- `prepare-model` is the only command permitted to download (D7). It reports
  profile ID, pinned revision, dimensions, artifact sizes, and cache path.
- `status` reports lexical/semantic availability; chunk, semantic-input, and
  vector counts; per-component text/vector/FTS/mapping bytes; sidecar, WAL, and
  model-cache bytes; configured warn/refuse budgets and filesystem headroom;
  schema/chunk/profile identity; and FTS5 integrity.
- `rebuild` explicitly transitions schema, chunk format, and—when prepared—the
  current binary's embedding profile. Lexical replacement is one in-place
  sidecar transaction; guarded vector fill follows in batches and never holds a
  write transaction during inference.
- `clear` holds the exclusive lifecycle lock, transactionally deletes sidecar
  contents, and truncates the WAL after auto-vacuum. If SQLite cannot open the
  sidecar, it recreates that disposable file under the same lock. Tasks,
  comments, `repo-link.db`, and model artifacts are untouched. The next search
  recreates literal+lexical state.

Progress (reconcile counts, embedding batches) goes to stderr; JSON goes to
stdout. `agents_intro.md` gains task search / search-index guidance and
`rl agents docs` is rerun (the command reference is hand-curated, not
generated from the clap tree).

## 4. Crate map

```text
app-cli
  └─ rl task search / search-index
       └─ application-search: TaskSearchService
            ├─ ports::TaskSearchSourceRepository
            │    └─ infra-sqlite: authoritative task/comment reads
            ├─ ports::TaskSearchIndex
            │    └─ infra-sqlite: repo-link.db.task-search.db
            └─ ports::EmbeddingProvider
                 ├─ infra-embed: candle + verified model cache
                 └─ testing-fixtures: deterministic fake embedder
```

## 5. Storage and lifecycle invariants

1. **Task-only corpus.** Only current task title/body/comment text is indexed.
2. **Derived and disposable.** Every search row is re-derivable from
   authoritative rows; `clear` removes all of them from the sidecar.
3. **No history.** Snapshots and prior chunk versions are never indexed.
4. **Authoritative isolation.** `repo-link.db` stores no search schema, text,
   FTS, or vectors and is never written by search.
5. **In-place transactions.** Routine sidecar updates are transactional and
   need no file replacement or cross-database acknowledgement protocol. Only
   explicit clear may recreate an unopenable sidecar under the exclusive
   lifecycle lock.
6. **FTS consistency by construction.** External-content triggers on the
   derived table are the only triggers this feature introduces; authoritative
   tables carry none.
7. **Explicit profile transitions.** Ordinary search never replaces a sidecar
   profile chosen by another binary.
8. **Complete semantic coverage.** Tokenizer-bounded semantic inputs cover all
   lexical chunk text; no runtime truncation is accepted.
9. **Observable and capped.** Status exposes every storage class and budget;
   preflight plus `max_page_count` refuses growth before disk pressure can
   affect authoritative task operations.

At 384 float32 dimensions raw vectors cost ~1.5 KiB per chunk: ~1.7 MiB at
the current ~1,150-chunk proxy, ~15 MiB at 10k chunks, and ~146 MiB at 100k
before tokenizer-driven semantic splitting. The measured complete lexical +
vector + FTS sidecar amplification was approximately 4.3 KiB per conservative
chunk: roughly 42 MiB at 10k similar chunks and 420 MiB at 100k. These are
planning estimates, not limits; D5 preflight, filesystem headroom, and
`max_page_count` enforce the numeric budgets selected before Stage 1.

## 6. Non-goals

- Managing or indexing repository files, ADR/RFC contents, pull requests,
  commits, source code, audit logs, task snapshots, or remote issues that are
  not tasks.
- Inferring meaning absent from all current indexed task text.
- A remote embedding API, hosted vector database, or standalone vector-store
  process.
- ANN indexes, quantization, HNSW, reranking, late interaction, or GPU
  inference in v1 (§7 names the upgrade path).
- Triggers, revision counters, or any change-tracking state on authoritative
  tables.
- Daemon-maintained index state (below the D6 ceiling there is no background
  maintenance to schedule; a per-search reconcile is milliseconds).
- Cross-user or server synchronization of search state.
- Automatically relating, editing, closing, or deduplicating tasks based on
  similarity.
- Persisting search text, FTS, or vectors in `repo-link.db`, task snapshots,
  events, or GitHub.
- Turning `task_search_chunks` into a general-purpose document abstraction.

## 7. Alternatives considered

### Trigger-based revision feed and rename-based sidecar rebuild

The 2026-07-21 draft specified: SQLite triggers on `tasks`/`task_comments`
incrementing a singleton revision and coalescing dirty task IDs; a
`<db-stem>.task-search.db` sidecar reconciled from revision deltas with a
replay-safe sidecar-first commit order and compaction watermark; an
interprocess maintenance lock; and an atomic temp-file rename swap for
rebuilds. Adversarial review confirmed the machinery was both defective as
specified and unnecessary at this scale:

- The atomic rename ignored the **active** sidecar's `-wal`/`-shm`. SQLite
  pairs a WAL with its database by filename, so a hot WAL left by a crashed
  process — or by a concurrent lock-free reader, which the draft explicitly
  permitted — is silently replayed into the freshly built index on the next
  open (the documented `howtocorrupt.html` class).
- The maintenance lock was unspecified, and the natural implementation
  (locking the sidecar file) is broken by the rename itself: file locks bind
  to inodes, so a swap lets two maintainers hold "the" lock concurrently.
- The triggers false-dirtied every task on every save, because task
  persistence is a full-row upsert: `UPDATE OF title, body` fires whenever
  the columns are assigned, not when they change.
- The FTS5 external-content maintenance discipline was unspecified; violations
  desync silently.
- The stem-derived sidecar name collided across sibling databases, and the
  sidecar carried no identity binding it to its source.
- All of it existed to avoid re-reading and re-hashing 0.91 MiB of text —
  milliseconds — while content addressing already prevented redundant
  embedding, which is the only expensive step.

The sidecar itself was not the defect. D5/D6 retain it but update it in place:
SQLite owns writer serialization and crash recovery, no file is renamed, the
full source snapshot is re-derived before each query, and no authoritative
revision or acknowledgement state exists. The stable sibling lifecycle lock
protects only explicit replacement of an unopenable sidecar; it is not an
application writer lock. Incremental tracking returns behind the same source
port only after the measured D6 ceiling is crossed.

### Search tables in `repo-link.db`, with or without auto-vacuum

Rejected. `auto_vacuum=FULL` could return freelist pages after deletes, but the
existing authoritative database was created with auto-vacuum disabled.
Enabling it requires a one-time `VACUUM` rewrite and additional pointer-map
metadata; it still places FTS, vectors, write amplification, WAL growth, and
derived-data high-water marks in authoritative backups. It also cannot bound
the external model cache. D5 enables full auto-vacuum on the new disposable
sidecar instead, where page movement and fragmentation cannot affect task
storage.

### Dense semantic search only

Rejected. Dense retrieval bridges paraphrases but can miss exact identifiers,
code symbols, or error strings. Literal and FTS5 lanes provide predictable
exact/lexical behavior while dense retrieval covers the semantic gap.

### FTS5 only

Rejected as the complete solution: it cannot bridge relevant wording with
little token overlap, which is the stated motivating gap. It is one lane in
D4 — and, deliberately, the whole of PR1 (§9), so useful search ships before
any model exists.

### Unconditional literal-first ordering

Rejected (previously accepted). On natural-language queries an incidental
substring match ("search index" mentioned in passing) would outrank the true
paraphrase answer. D4 keeps the hard guarantee where exactness is signalled by
`--exact` or identifier-shaped input and demotes the literal flag to fusion
evidence elsewhere.

### Literal lane over formatted chunk text

Rejected (previously accepted). Non-overlapping chunking breaks exact
matching for long strings straddling a boundary — precisely the error-string
use case the lane exists for — and injected field labels create false
matches. Raw authoritative text is the same order of scan cost with neither
artifact.

### Count and `max(updated_at)` fingerprints; append-only change logs; per-comment invalidation

All superseded by per-search hash-diffing, which needs no fingerprint, no
log, and no invalidation unit at all.

### Late interaction (ColBERT-style) and cross-encoder reranking

The strongest quality upgrade at this corpus size — exhaustive MaxSim over
token vectors is tens of milliseconds at 519 tasks, and a small calibrated
cross-encoder over a top-40 pool adds principled no-answer signalling.
Rejected for v1 on cost: a second pinned artifact doubles the supply-chain
surface, ort-class runtimes conflict with offline builds, token matrices grow
the index ~7×, and every calibration constant depends on a labelled fixture
that must exist and be trusted first. This is the named follow-up once the
§10 fixture exists and shows the single-vector lane leaving quality on the
table.

### Daemon-maintained search index

Rejected, and more strongly than before: with per-search reconciliation
costing milliseconds there is no background maintenance to schedule at any
foreseeable scale. The named future daemon contribution is different — a
warm `embed_query` endpoint in `rld` (unix socket, second `EmbeddingProvider`
impl with in-process fallback) if the measured per-invocation model load
(§10) proves unacceptable.

### `ort` / onnxruntime as the inference runtime

Rejected for v1. The default feature downloads prebuilt binaries in
`build.rs`, violating offline deterministic builds; the vendored alternative
requires cmake and a C++ toolchain on every contributor machine and CI
runner. Candle's selected features avoid build-time binary downloads; Stage
2/4 measures its CPU and cold-load cost. The port keeps the choice reversible.

### LanceDB / sqlite-vec / tantivy

Rejected for v1. Current scale requires neither a vector store's versioning
lifecycle, a C extension plumbed through sqlx, nor a second on-disk index
format with segment merging. Raw BLOB scan plus FTS5 is sufficient,
inspectable, and disposable.

### Remote embedding API

Rejected. Sending task/comment content externally conflicts with the
local-first default and makes offline search impossible.

### Eager embedding on every task/comment mutation

Rejected. Ordinary task correctness must not depend on model availability,
and a local edit must not wait for CPU inference. Reconciliation happens at
search time, when the user has expressed interest in fresh results.

## 8. Risks and mitigations

- **Model quality.** General embeddings may collapse repo-specific
  distinctions. Mitigation: the per-category labelled fixture and relative
  gates in §10, run before pinning and before any profile swap.
- **Exact identifiers.** Dense models may tokenize opaque strings poorly.
  Mitigation: the literal lane runs over raw text, and identifier-mode
  queries keep the literal-first hard guarantee independent of embeddings.
- **Language coverage.** The English-first candidate set may fail on
  multilingual tasks. Mitigation: represent actual task languages in the
  fixture; `multilingual-e5-small` is the named fallback candidate.
- **O(corpus) per search.** The reconcile pass grows with total corpus, not
  the delta. Mitigation: measured cost target and a named ceiling (D6) with
  an additive upgrade path behind the same port.
- **Per-invocation model load.** A fresh CLI process pays model
  deserialization on every semantic query, and it plausibly dominates
  end-to-end latency. Mitigation: it is measured explicitly as an acceptance
  gate (§10), mmap-able safetensors keeps it small, and the named escape is a
  warm `embed_query` endpoint in `rld` (§7).
- **Semantic input length.** Model-independent lexical chunks may exceed the
  active model's effective token budget. Mitigation: D2 derives complete
  tokenizer-bounded semantic inputs and validates coverage before inference;
  overlong queries skip the semantic lane rather than truncate.
- **Concurrent duplicate work.** Two CLI processes reconciling after the same
  edit burst can embed the same semantic input. Mitigation: identity/profile
  guards discard stale results; duplicate inference wastes CPU but cannot
  attach a vector to different text.
- **Sidecar growth.** Text, FTS, and semantic segments amplify source bytes.
  Mitigation: conservative preflight, filesystem headroom, auto-vacuum,
  `max_page_count`, cache budgets, and observable warn/refuse thresholds.
- **Mixed binaries.** Different chunk or profile versions could otherwise
  rebuild each other's state. Mitigation: ordinary search never changes
  non-NULL format/profile ownership; explicit rebuild is the only transition.
- **Sidecar corruption.** Search-index faults must not affect task data.
  Mitigation: a separate file, FTS integrity checks, literal-only degradation,
  in-place rebuild when SQLite remains openable, and explicit locked
  recreation otherwise.
- **Model supply chain.** Mutable upstream artifacts could change output or
  introduce unverified files. Mitigation: in-binary digest manifest (no
  TOFU), safetensors-only artifacts, atomic verified install into a
  content-addressed cache.
- **FTS expression injection.** Mitigation: the exact construction is
  specified (D4) — tokenize, double-quote terms with internal quotes doubled,
  join with OR, bind as a parameter — and tested with hostile inputs.
- **Misleading scores.** Users may read cosine or fused scores as confidence.
  Mitigation: expose lane metadata, call them ranking signals, never label
  them probabilities.

## 9. Staged implementation plan

### Stage 0 — Reconciliation and storage spike

- Build the exact D5 sidecar schema against current plus synthetic 10k/100k
  chunk corpora.
- Measure zero-change and changed full-hash reconciliation, sidecar/WAL peak,
  `auto_vacuum=FULL` plus truncating-checkpoint clear-time reclamation, FTS
  amplification, and `max_page_count` failure behavior.
- Exercise serialized concurrent reconciles, a source edit during reconcile,
  ROWID reuse during an embedding batch, mixed-version metadata, and exclusive
  recovery of an unopenable sidecar while another process holds a shared
  lifecycle lock.
- Select and record numeric sidecar, WAL/headroom, and warn/refuse budgets
  before PR1 begins.

### Stage 1 — PR1: search without a model

- Sidecar schema: metadata, lexical chunks, semantic-input vectors, FTS5,
  immutable-content triggers, `auto_vacuum=FULL`, and `max_page_count`; no
  authoritative database migration. Add FTS5/DBSTAT smoke tests.
- D2/D3 chunker with `chunk_format_version`; SHA-256 hashing.
- `application-search` with serialized sidecar reconciliation, storage
  preflight, query-mode classifier, explicit `--exact`, literal + lexical
  lanes, fusion, excerpt verification, and `dto-shared` result contracts.
- `rl task search` and `search-index status|rebuild|clear` under the final
  CLI/JSON contract, with `"lexical_available": true` and
  `"semantic_available": false`.
- Full deterministic test coverage (§10). Ships alone and is immediately
  useful for exact identifiers, error strings, and keyword recall.

### Stage 2 — Fixture and profile selection

- Build the reviewed, synthetic-or-sanitized labelled fixture (§10
  categories; raw local task content is never committed).
- Run literal/FTS baselines, then the named candidate models via candle.
- Select and pin the profile by the predeclared relative gates; author the
  manifest and digests into the source tree.
- Measure complete semantic-input amplification and choose the numeric
  model-cache budget before PR2 acceptance.

### Stage 3 — PR2: semantic lane

- `infra-embed` (candle adapter, verified cache, `prepare-model`).
- Complete tokenizer-bounded semantic-input planning, guarded vector batches,
  semantic lane, and three-lane fusion.
- No change to any PR1 shape; `prepare-model` and the semantic lane are
  purely additive.

### Stage 4 — Measure and record

- Record: reconcile pass p50/p95 (zero-change and bursty), end-to-end cold
  `rl task search` wall time including process start and model load, rebuild
  time, per-component sidecar/WAL bytes, semantic-input amplification, model
  cache bytes, and budget headroom.
- Fold the numbers into this RFC. ANN, incremental tracking, reranking, or the
  `rld` warm-embed endpoint are follow-up RFCs gated on measured ceilings.

## 10. Testing and evaluation strategy

### Deterministic unit and integration tests (offline, fake embedder)

Chunking and hashing:

- Short title/body produces one core chunk; empty body produces title-only
  content.
- Long bodies/comments chunk without overlap and without dropping text;
  oversized paragraphs fall back to sentence/valid-UTF-8-boundary splitting.
- An oversized title stays fully indexed; body chunks carry the marked,
  deterministic bounded anchor.
- Comment author/timestamp never appear in indexed text.
- Chunking is deterministic across runs and platforms;
  `chunk_format_version` mismatch never rewrites implicitly.
- Tokenizer-derived semantic inputs fit the effective model budget and cover
  every lexical chunk byte without runtime truncation.

Reconciliation:

- Create/edit/delete of tasks and comments converge the index in one
  reconcile; a zero-change reconcile performs zero writes.
- Wholesale comment refresh with unchanged bodies (current persistence
  behaviour) produces zero index writes and zero re-embeds.
- Task deletion removes all its rows, vectors, and FTS entries.
- Metadata-only edits (lifecycle, workspace, repo, priority, assignees)
  produce zero index writes.
- Concurrent reconciles acquire the sidecar transaction before their source
  snapshot; a slow older process cannot regress a newer committed index.
- Snapshots never produce search rows.
- Duplicate identical comment bodies on one task collapse to one row and
  keep the zero-writes property.
- A deleted/reused ROWID cannot accept a stale vector because task ID, content
  hash, embedding-input hash, and profile guards must all match.
- Alternating binaries with different chunk/profile versions degrade without
  mutating or thrashing the sidecar; only explicit rebuild changes ownership.
- Embedding failure leaves committed vectors serving and missing semantic
  inputs retryable.
- Sidecar path derivation distinguishes `foo.db` from `foo.sqlite`.
- Sidecar clear takes the exclusive lifecycle lock, shrinks the database and
  WAL under full auto-vacuum plus a truncating checkpoint, and leaves
  `repo-link.db` byte-for-byte and schema-for-schema unchanged.
- Unopenable-sidecar recovery waits for existing shared lifecycle locks, then
  recreates the sidecar and its WAL companions without exposing mixed
  generations to a concurrent command.
- Projected sidecar, WAL, free-space, and model-cache budget violations refuse
  before growth; `max_page_count` rolls back an attempted overrun.

Retrieval:

- Literal lane matches across what would be chunk boundaries and never
  matches the injected field labels (raw-text scan).
- Query-mode classification covers explicit `--exact`, identifier shapes,
  natural-language queries, and mixed identifier+prose queries; shell quoting
  alone does not alter mode.
- FTS expression construction: quotes, operators, punctuation, and code
  symbols in user input cannot alter SQL or FTS syntax (hostile-input
  corpus).
- Identifier tokens (`snake_case`, `dashed-name`) survive the FTS tokenizer.
- Lane collapse to task level and RRF fusion are deterministic; ties resolve
  by task ID.
- A source changed after reconciliation is omitted during authoritative result
  verification and repaired by the next query.
- Degraded mode: no profile prepared → literal+lexical results,
  `semantic_available: false`, stderr hint.
- Schema/chunk mismatch → literal-only results, `lexical_available: false`, and
  explicit rebuild guidance without mutation.
- Overlong query → literal+lexical results and
  `semantic_skipped_reason: "query_too_long"`; no truncation.
- Comment evidence: `remote_comment_id` reported when unambiguous, omitted
  otherwise.
- Search and reconcile perform no network access under any state.
- The top-level JSON wrapper includes query mode, lane availability, optional
  semantic skip reason, and results.
- `status`/`rebuild`/`clear`/`prepare-model` emit JSON and touch only their
  documented sidecar/cache state; FTS5 `integrity-check` passes after every
  mutation test.
- FTS5 availability smoke test (in-memory virtual table) pins the bundled
  build configuration.

### Retrieval-quality evaluation (Stage 2, opt-in real models)

The checked-in fixture is synthetic or explicitly reviewed and sanitized. It
contains task content, queries, and labelled relevant task IDs, with
**predeclared minimum counts per category**:

- exact identifiers, phrases, code symbols, and error strings (≥20);
- paraphrases with little or no token overlap (≥30);
- misleading generic software terms (≥15);
- long descriptions whose relevant paragraph is not first (≥10);
- tasks with many irrelevant comments (length bias) (≥10);
- typo'd near-misses of identifiers and words (≥10);
- supported task languages (≥10 per language);
- closed historical tasks (≥10).

Gates are **relative and per-category**, not aggregate-absolute:

1. exact-match retention is 100% — no candidate may lose a literal/identifier
   query that the Stage 1 baseline answers;
2. fused results are non-regressive versus the FTS-only baseline on every
   category;
3. the semantic lane beats the FTS-only baseline on the paraphrase category
   by a predeclared margin (this is the entire justification for the lane —
   if no candidate clears it, PR2 does not ship until one does);
4. report Recall@10 and MRR per category with bootstrap confidence intervals;
   a candidate is accepted only when its paraphrase gain exceeds the noise
   band. A model is not accepted merely for being the best of a weak set.

Real-model tests are opt-in and use only explicitly prepared local profiles.
Normal CI downloads nothing and uses the deterministic fake.

### Performance evaluation (Stage 4)

- Reconcile pass p50/p95: zero-change, single-edit, bursty (100 edits), at
  current scale and synthetic 10k/100k-chunk scales.
- **End-to-end cold `rl task search` wall time — process start to JSON on
  stdout — with and without the semantic lane.** Model load time reported
  separately. This, not the in-process vector scan, is the latency
  acceptance gate, and the bound is predeclared: literal+lexical-only p95
  ≤ 300 ms; semantic p95 ≤ 2 s. Exceeding the semantic bound triggers the
  named remedies in order — a smaller candidate model, then the `rld`
  `embed_query` endpoint (§7).
- Full rebuild time and peak memory; per-component bytes via `dbstat`.
- Sidecar, WAL peak, clear-time reclamation, and model-cache budgets are
  re-stated against measured numbers.

## 11. Open questions

### Resolved

- **Corpus:** current task title/body/comments only.
- **Freshness:** per-search hash-diff reconcile; no authoritative-table
  triggers or revisions; SQLite serializes in-place sidecar reconciles;
  incremental tracking is deferred behind a measured ceiling.
- **Storage:** size-capped `repo-link.db.task-search.db` sidecar with full
  auto-vacuum; no search schema or bytes in `repo-link.db`.
- **Retrieval:** literal (raw text) + FTS5/BM25 + exact cosine, query-mode
  conditional ordering, explicit `--exact`, and RRF k=60 default.
- **Comment identity:** not a search dependency; content addressing absorbs
  surrogate churn; `remote_comment_id` recovered at render time.
- **Runtime:** candle, committed. **Trust root:** in-binary digests.
  **Cache:** global, content-addressed, and hard-budgeted.
- **Versioning:** ordinary search never changes an incompatible chunk/profile
  owner; explicit rebuild transitions it.
- **Degraded mode:** literal always works; lexical/semantic availability is
  explicit and incompatible states do not mutate automatically.
- **Semantic coverage:** tokenizer-bounded inputs cover all lexical text; no
  runtime truncation.
- **Default result count:** 10. **Pending comments:** searchable immediately.

### Remaining

1. Which of the named candidate models clears the §10 per-category gates, and
   with which instruction prefixes and batch size? (Stage 2.)
2. Final RRF constant and FTS `OR`-vs-`AND` expression policy after fixture
   runs. (Stage 2.)
3. What numeric sidecar, WAL/headroom, and global model-cache warn/refuse
   budgets follow from the Stage 0/2 measurements?
4. Whether measured cold-invocation latency justifies the `rld`
   `embed_query` endpoint. (Stage 4.)

## 12. References

- SQLite, "How To Corrupt An SQLite Database File" — the
  hot-journal/WAL-mispairing class that motivated abandoning the sidecar
  rename swap: <https://www.sqlite.org/howtocorrupt.html>.
- SQLite auto-vacuum and VACUUM document why existing authoritative databases
  do not shrink after DELETE without a rewrite, and why D5 enables full
  auto-vacuum only on the new sidecar:
  <https://www.sqlite.org/pragma.html#pragma_auto_vacuum> and
  <https://www.sqlite.org/lang_vacuum.html>.
- SQLite WAL checkpoint modes define the truncating checkpoint used by clear:
  <https://www.sqlite.org/pragma.html#pragma_wal_checkpoint>.
- SQLite FTS5 — BM25, external-content tables and their trigger maintenance
  pattern, trigram tokenizer: <https://sqlite.org/fts5.html>.
- Reciprocal rank fusion combines independent result rankings without score
  calibration: <https://doi.org/10.1145/1571941.1572114>.
- Sentence Transformers — asymmetric query/corpus instructions and exact
  corpus scans for small corpora:
  <https://www.sbert.net/examples/sentence_transformer/applications/semantic-search/README.html>.
- SQLite Vec1 intro — exact nearest-neighbour recommended below ~5,000
  vectors: <https://sqlite.org/vec1/doc/trunk/doc/vec1intro.md>.
