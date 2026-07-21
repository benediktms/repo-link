# RFC 0007 — Local semantic search over task content

Status: Draft (2026-07-21)
Tracking epic: **#TBD**

## 1. Context

repo-link can filter and list tasks by structured fields, but it cannot find a
task from a concept expressed with different words. The intended query is not
"find this token quickly"; it is "find tasks about this idea":

- a query describing a feature should find implementation, follow-up, and
  design tasks whose wording differs;
- a query using an ADR/RFC name should find nearby implementation tasks when
  the task corpus contains enough shared context to place them together; and
- searching either side of a semantic cluster should return the other because
  both query and task content occupy the same embedding space.

Full-text search does not close that lexical gap. Dense embeddings plus cosine
similarity do, within an important boundary: **the model only sees indexed task
text**. A task containing only an opaque local identifier such as `RFC-0002`
does not acquire the unpublished meaning of that RFC. Clustering emerges when
the title, body, or comments contain shared semantic context; vector search is
direct similarity, not transitive graph traversal.

### Current persistence shape

- `tasks.title` and `tasks.body` hold the current task summary and description.
- `task_comments` stores current synced and pending comments on a separate
  persistence axis; comment writes do not append task snapshots or change
  `sync_state`.
- `TaskRepository::list` deliberately skips comments, while point reads hydrate
  them.
- `task_snapshots` contains append-only history and repeats title/body over
  time. Snapshot history is audit data, not search input.
- SQLite is authoritative for task state. Any semantic index must remain a
  disposable projection that cannot affect task correctness or sync.

### Capacity observation

On 2026-07-21 the local database contained 503 current tasks, 121 current
comments, and 3,738 snapshots. Ignoring long-body chunking, one task-core vector
per task plus one vector per comment is 624 vectors. At 384 float32 dimensions,
the raw vector payload is approximately 0.91 MiB:

```text
vector_bytes = unique_chunks × dimensions × bytes_per_component
             = 624 × 384 × 4
             = 958,464 bytes
```

The embedding model itself is likely to consume more disk than the initial
vectors. Storage becomes dangerous through unbounded chunks, duplicated
embeddings, retained model versions, or versioned vector-store files—not from
the current task count alone.

## 2. Goals

1. Search current task title, body, and comments by semantic similarity.
2. Keep title and body together so the title anchors the description's meaning.
3. Preserve useful context when long bodies or comments require chunking.
4. Keep all embedding computation local after explicit model installation.
5. Bound storage growth to unique current chunks for one active model.
6. Keep the semantic index disposable, inspectable, and rebuildable.
7. Return ordinary task results with enough match metadata to explain why they
   ranked.

## 3. Decisions

### D1 — Search only current task content

The v1 corpus is exactly:

- current task title;
- current task body; and
- current synced and pending comment bodies.

The index excludes task snapshots, audit events, outbox payloads, repository
documents, ADR/RFC files, issues not imported as tasks, pull requests, commits,
and source code.

Closed tasks remain searchable by default. Finding prior implementations and
decisions is a primary semantic-search use case; silently restricting results
to open work would remove much of the useful corpus. Workspace, repo, and
lifecycle filters are applied from current authoritative task state.

### D2 — Title and body form the core semantic document

For a short task, construct one core document with stable field labels:

```text
Title: <task.title>

Description:
<task.body>
```

Title and body are not embedded independently. The title supplies the task's
subject; the body supplies detail. Averaging or ranking independent title/body
vectors would discard that relationship.

If the core document exceeds the selected model's useful input limit, split the
body on paragraph boundaries and prepend the complete title to every chunk:

```text
Title: <task.title>

Description (part N):
<body paragraph chunk>
```

V1 uses no overlapping chunks. Paragraph-preserving boundaries plus the
repeated title retain context without duplicating an arbitrary overlap window.
An overlap may be added only if the evaluation set demonstrates boundary
misses.

An empty body still produces one title-only core document.

### D3 — Comments are separate, title-anchored documents

Each comment is embedded separately:

```text
Title: <task.title>

Comment:
<comment.body>
```

Author and timestamp are omitted because they are metadata, not task meaning.
A long comment follows D2's paragraph chunking rule, with the complete title
prepended to every chunk.

The implementation must not concatenate the entire comment thread into the
core document. Threads can be long, repetitive, or operational; merging them
would dilute the task's primary meaning and make every comment change
re-embed the whole task.

### D4 — Exact cosine search and max-score task roll-up

The query is embedded with the same model and model-specific query instruction
used by the indexed documents. All stored vectors are normalized, and v1 uses
exact cosine similarity; no ANN index is created.

Each chunk produces a similarity score. A task's score is the maximum score of
its eligible chunks:

```text
task_score(task) = max(cosine(query, chunk) for chunk in task.chunks)
```

Max-score roll-up lets one focused description paragraph or comment retrieve a
task without unrelated task text dragging down an average. The result records
the winning chunk's `source_kind` (`core` or `comment`) and source identity.

There is no fixed minimum similarity threshold in v1. Scores vary by model and
corpus; the command returns the top `--limit` tasks in descending score order.
Ties are resolved by task ID for deterministic JSON.

Approximate indexing, reranking, multi-vector late interaction, and learned
score fusion require measured quality or latency evidence before introduction.

### D5 — The semantic index is a disposable SQLite sidecar

The authoritative `repo-link.db` schema receives no embedding columns or vector
tables. Store the derived index in a sibling database:

```text
<authoritative-db-stem>.search.db
```

For the default path this is `repo-link.search.db`. For `rl --db /tmp/test.db`,
the sidecar is `/tmp/test.search.db`. No independent path configuration is
introduced in v1.

The sidecar is:

- safe to delete at any time;
- excluded from task backup, sync, snapshots, and authoritative-database
  migrations;
- rebuilt from the authoritative database;
- limited to one active embedding model and dimension; and
- never consulted by task mutation or GitHub reconciliation.

Search unavailability or embedding failure must not prevent task creation,
editing, comment sync, daemon polling, or shutdown.

### D6 — Content-address embeddings and map them to task chunks

Store each unique formatted chunk once per active model. The sidecar has three
logical tables:

```text
search_meta(
    schema_version,
    model_id,
    dimensions,
    built_at
)

embeddings(
    content_hash PRIMARY KEY,
    vector BLOB NOT NULL
)

task_chunks(
    task_id,
    source_kind,
    source_id,
    chunk_index,
    content_hash REFERENCES embeddings(content_hash),
    PRIMARY KEY(task_id, source_kind, source_id, chunk_index)
)
```

`content_hash` is the SHA-256 digest of the exact formatted UTF-8 input in
D2/D3. Because one sidecar contains only one active model, `model_id` does not
need to repeat on every row. Identical chunks share one vector; mapping rows
remain cheap.

`source_id` is the stable literal `core` for a task's title/body source. For a
comment it is the remote comment ID when present, otherwise the comment's
stable local surrogate ID. It must not depend on comment position: inserting a
new comment must not invalidate every later mapping.

Vectors are stored as fixed-length, little-endian float32 BLOBs. The adapter
rejects a vector with the wrong dimension or a non-finite component. Exact
search streams vectors and keeps a bounded top-k candidate set; it does not
load the complete vector corpus into memory.

The sidecar does not duplicate title, body, or comment text. Result rendering
reads current content from the authoritative repository by task/source ID.

### D7 — Reconcile lazily at search time

Task writes do not synchronously embed text and the daemon does not maintain a
second queue. Before executing a search, the semantic-search application
service:

1. reads a lightweight projection of current tasks and comments in scope;
2. constructs D2/D3 chunks and content hashes;
3. replaces stale `task_chunks` mappings;
4. embeds only hashes absent from `embeddings`;
5. removes unreferenced embeddings; and
6. runs the query against the reconciled index.

Before step 4, reconciliation estimates the resulting sidecar size from the
number of unique chunks, the active dimension, and a conservative SQLite
overhead factor measured in Stage 0. It refuses the build before model
inference or sidecar growth if the configured storage budget would be
exceeded. Stage 0 selects the default numeric budget, but the production path
must not ship without this preflight guard.

This makes a completed search current as of its source read without coupling
normal task writes to model inference. A task edited concurrently after that
read may appear with its previous embedding for one search and is corrected by
the next reconciliation; the semantic sidecar is a cache, not an authority.

If `search_meta.model_id`, dimensions, or schema version differ from the active
runtime, delete and rebuild the sidecar. Do not retain parallel model versions.

### D8 — Local model only; exact model selected by an evaluation spike

V1 does not send title, body, comments, or queries to a remote embedding API.
The embedding adapter is local and performs no network access after explicit
model installation.

The selected model must:

- have a license compatible with repo-link distribution;
- support local CPU inference on macOS and Linux;
- produce at most 384 dimensions, or support validated dimensionality
  truncation to at most 384;
- document any distinct query/document instruction prefixes;
- expose its tokenizer or reliable input-limit accounting for D2/D3; and
- demonstrate useful task retrieval on the evaluation set in §10.

The RFC deliberately does not choose the model or inference runtime before the
spike. Model choice determines binary dependencies, download size, dimensions,
chunk limits, latency, and retrieval quality; guessing it in the architecture
record would make those constraints fictional.

### D9 — Add two infrastructure ports, not a vector-store service

The application layer owns source construction, reconciliation policy,
filtering, and task-level roll-up. Infrastructure supplies two external
capabilities:

```text
EmbeddingProvider
  model_id()
  dimensions()
  embed_documents(texts)
  embed_query(query)

TaskSearchIndex
  metadata()
  reconcile_mappings(...)
  missing_hashes(...)
  store_embeddings(...)
  search_exact(query_vector, eligible_task_ids, limit)
  prune_unreferenced()
  clear()
  stats()
```

`application-query` gains the semantic task-search use case. `infra-sqlite`
implements the sidecar index; the selected local inference runtime lives in a
small infrastructure adapter rather than leaking into application or domain
crates. `testing-fixtures` provides deterministic fake implementations.

`TaskRepository` gains one batched lightweight search-source read so the
application does not call `get` once per task merely to hydrate comments. This
projection returns only the fields needed for chunk construction and current
filtering; it does not hydrate relations, baselines, or snapshots.

No semantic-search type enters `domain-task`. Embeddings, chunk IDs, model IDs,
and scores are derived query concerns, not task invariants.

### D10 — CLI surface and JSON contract

The user-facing query is:

```text
rl task search <query> [--workspace <id>] [--repo <handle>]
                       [--status open|closed|all] [--limit <N>]
```

`--workspace` and `--repo` use the existing cwd-aware resolvers. Search defaults
to `--status all` and a bounded result count chosen at implementation time.
`--limit 0` and an empty/whitespace-only query are rejected.

Each JSON result contains:

```text
{
  "id": "rpl-abc",
  "task_id": "<uuid>",
  "title": "...",
  "score": 0.82,
  "matched_source": {
    "kind": "core|comment",
    "source_id": "...",
    "chunk_index": 0
  }
}
```

`score` is a ranking signal, not a calibrated probability, and is comparable
only within results produced by the same model/index version.

Index maintenance is explicit and JSON-emitting:

```text
rl task search-index status
rl task search-index rebuild
rl task search-index clear
```

`status` reports model ID, dimensions, task/chunk/unique-vector counts, raw
vector bytes, sidecar file bytes, model-cache bytes when discoverable, and last
build time. `rebuild` removes the old derived sidecar before rebuilding so it
does not require temporary space for two complete indexes. `clear` removes only
derived search state and never touches authoritative task data.

Model installation UX remains an open question (§11); normal `task search`
must not silently download a large model.

## 4. Crate map

```text
app-cli
  └─ rl task search / search-index
       └─ application-query: SemanticTaskSearchService
            ├─ ports::TaskRepository search-source projection
            │    └─ infra-sqlite: repo-link.db
            ├─ ports::EmbeddingProvider
            │    └─ infra embedding adapter: local model runtime
            └─ ports::TaskSearchIndex
                 └─ infra-sqlite: repo-link.search.db
```

DTOs crossing into JSON live in `dto-shared` or `application-query::dto`,
following the existing query-row convention. The CLI remains a thin
composition/dispatch layer.

## 5. Storage and lifecycle invariants

1. **One active model.** A model or dimension change replaces the sidecar; it
   never appends another embedding column or retained version.
2. **One vector per unique current chunk.** `content_hash` deduplicates exact
   formatted content.
3. **No history.** Snapshots and previous chunk versions are never indexed.
4. **No source-text copy.** Canonical text remains in `repo-link.db`.
5. **No ANN copy.** V1 has no second graph/PQ/HNSW index alongside raw vectors.
6. **Reclaimable.** `search-index clear` returns vector storage to zero;
   rebuild creates a fresh sidecar rather than preserving free pages forever.
7. **Observable.** `search-index status` makes storage amplification visible
   before it becomes surprising.

At 384 float32 dimensions:

| Unique chunks | Raw vector payload |
|---:|---:|
| 624 | 0.91 MiB |
| 10,000 | 14.6 MiB |
| 100,000 | 146 MiB |
| 1,000,000 | 1.43 GiB |

Actual sidecar size also includes SQLite pages, hashes, and mappings. It should
remain a small multiple of raw vector bytes, not an unbounded multiple. A hard
default storage budget is not chosen until the model/chunking spike measures
real amplification (§11).

## 6. Non-goals

- Full-text/BM25 search or hybrid lexical+dense ranking.
- Indexing repository documents, ADR/RFC contents, pull requests, commits,
  source code, audit logs, task snapshots, or remote issues that are not tasks.
- Inferring meaning absent from all indexed task title/body/comment text.
- A remote embedding API or hosted vector database.
- A standalone vector-store process.
- ANN indexes, product quantization, HNSW, reranking, or GPU inference.
- Cross-user/server synchronization of the semantic sidecar.
- Persisting embeddings in task JSON, snapshots, GitHub Issues, or events.
- Automatically relating, editing, or deduplicating tasks based on similarity.

## 7. Alternatives considered

### FTS5 only

Rejected as the solution to this RFC. FTS5 is compact and useful for exact
terms, but it cannot intentionally bridge semantically similar wording when
the terms do not overlap.

### One embedding for title, one for body

Rejected. It loses the subject/detail relationship the task author created.
D2 keeps them together and repeats the title only when long-body chunking is
required.

### One embedding for title, body, and all comments

Rejected. A growing discussion thread would dilute the core task, hit model
input limits, and require re-embedding all content after any comment change.

### Embedding columns in `repo-link.db`

Rejected. Embeddings are large, model-specific, replaceable derived data. They
do not belong in the authoritative task database or its backup lifecycle.

### LanceDB or another vector store

Rejected for v1. Current scale does not need a vector-store dependency, index
version retention, compaction, or a separate storage format. A simple SQLite
sidecar plus exact cosine search gives deterministic storage proportional to
unique current chunks.

### Eager re-embedding on every task/comment mutation

Rejected. It makes ordinary task correctness depend on model availability and
turns a local edit into potentially slow inference. D7 reconciles lazily at the
read boundary.

### Daemon-maintained semantic index

Rejected for v1. It adds another background lifecycle, recovery path, and
shutdown concern before search volume or latency requires one.

### Remote embedding API

Rejected for v1. Sending task/comment content externally conflicts with the
local-first default and makes offline search impossible.

## 8. Risks and mitigations

- **Model quality.** General embedding models may cluster broad software terms
  while missing repo-specific distinctions. Mitigation: the model spike and
  labelled evaluation gate in §10.
- **Opaque identifiers.** `RFC-0002` has no hidden local meaning to the model.
  Mitigation: state the corpus boundary plainly; search only claims similarity
  supported by task text.
- **Long-content dilution/truncation.** A model may silently truncate bodies or
  comments. Mitigation: tokenizer-aware paragraph chunking and title repetition.
- **Chunk explosion.** Small limits or overlap can multiply vectors. Mitigation:
  no overlap in v1, content-hash dedupe, and index stats.
- **First-search latency.** Reconciliation may embed the whole workspace.
  Mitigation: explicit model preparation, batched inference, progress on
  stderr, JSON only on stdout.
- **Model upgrades.** Scores and vector dimensions are incompatible across
  models. Mitigation: one active model; refuse search on a metadata mismatch
  and rebuild a fresh sidecar without mixing versions.
- **Concurrent task edits.** A search can race an edit after its source read.
  Mitigation: the index is derived and the next search reconciles; no task data
  is lost or overwritten.
- **SQLite file growth after churn.** Deleted pages may remain allocated.
  Mitigation: `rebuild` creates a fresh sidecar and `clear` removes it; periodic
  in-place vacuuming is unnecessary in v1.
- **Misleading scores.** Users may read cosine similarity as confidence.
  Mitigation: document score semantics and never label it a probability.
- **Inference dependency size.** The runtime/model can dwarf vector storage.
  Mitigation: include model-cache bytes in status and make installation
  explicit.

## 9. Staged implementation plan

### Stage 0 — Model, quality, latency, and storage spike

- Export current task title/body/comments using the D2/D3 formatting rules.
- Evaluate a small set of eligible local models at no more than 384 dimensions.
- Record model download size, runtime dependency size, indexing throughput,
  query latency, Recall@10/MRR, raw vector bytes, and actual sidecar bytes.
- Choose the model, inference runtime, tokenizer limits, batch size, and default
  result limit.
- Resolve the installation UX and default storage budget open questions.

No production schema or CLI contract ships before this evidence exists.

### Stage 1 — Ports, DTOs, and deterministic fixtures

- Add the lightweight task search-source projection to `TaskRepository`.
- Add `EmbeddingProvider` and `TaskSearchIndex` ports.
- Add `SemanticTaskSearchService` and result DTOs.
- Implement deterministic fake embeddings/index behavior in
  `testing-fixtures`.

### Stage 2 — SQLite sidecar and local embedder

- Add sidecar open/schema/reconcile/search/stats operations.
- Add the selected local embedding adapter.
- Implement D2/D3 formatting, token-aware chunking, content hashing, batched
  embedding, exact cosine scan, roll-up, and pruning.

### Stage 3 — CLI and operational surface

- Add `rl task search` and `rl task search-index`.
- Wire cwd-aware workspace/repo resolution and current filters.
- Add explicit model preparation, rebuild, clear, status, and error guidance.
- Refresh `rl agents docs` if command help changes the generated agent guide.

### Stage 4 — Measure before extending

- Run the labelled query set against the finished path.
- Record p50/p95 reconciliation and query latency plus index amplification.
- Add ANN, quantization, reranking, background indexing, or hybrid search only
  through a follow-up RFC backed by observed need.

## 10. Testing and evaluation strategy

### Deterministic unit/integration tests

- Title and body produce one core document when within the token limit.
- Empty body produces a title-only core document.
- Every long-body chunk repeats the title and no chunk overlaps in v1.
- Each comment is separate and title-anchored; author/timestamp are omitted.
- Identical formatted chunks share one stored vector.
- Reconciliation is idempotent when task content is unchanged.
- A body edit replaces only that task's core mappings and prunes orphaned
  vectors.
- Comment add/push/replace/delete converges to current comment rows.
- Snapshots never create semantic chunks.
- A model/dimension/schema mismatch discards and rebuilds the sidecar.
- Search filters by current workspace/repo/lifecycle state.
- Task score is the maximum eligible chunk score with deterministic ties.
- Search/index failure never mutates or blocks authoritative task operations.
- `status`, `rebuild`, and `clear` report JSON and affect only the sidecar.

### Retrieval-quality evaluation

Create a checked-in, text-only evaluation fixture containing representative
task documents, queries, and labelled relevant task IDs. It must include:

- paraphrases with little or no token overlap;
- clusters containing design, implementation, bug, and follow-up language;
- short opaque identifiers with and without surrounding semantic context;
- misleading generic software terms;
- long descriptions and comments whose relevant paragraph is not first; and
- closed historical tasks.

For each candidate model/dimension, report at least Recall@10 and MRR. The spike
must also inspect false positives/negatives; a single aggregate score cannot
show whether repo-specific vocabulary collapsed into generic similarity.

Real-model tests are opt-in and do not download models in normal CI. CI uses a
deterministic fake so builds remain offline and reproducible.

## 11. Open questions

1. Which local model and Rust inference runtime meet D8 and the §10 evaluation?
2. Should the selected model use 256 or 384 dimensions?
3. What explicit command installs the model without making ordinary search
   silently download a large artifact?
4. What tokenizer-derived input limit and batch size fit supported machines?
5. What default `--limit` gives useful recall without noisy output?
6. What sidecar-size budget should warn or refuse before indexing, and how is
   the estimate surfaced in JSON?
7. Should pending local comments be included immediately (recommended) or only
   after they are mirrored remotely?
8. Should index reconciliation cover only the selected workspace or all
   workspaces in the authoritative database?

## 12. References

- SQLite Vec1 currently recommends exact nearest-neighbour mode for relatively
  small vector sets (approximately fewer than 5,000) and requires trained ANN
  models at larger scale: <https://sqlite.org/vec1/doc/trunk/doc/vec1intro.md>.
- SQLite FTS5 provides BM25 lexical ranking but does not replace dense semantic
  retrieval: <https://sqlite.org/fts5.html>.
- Sentence Transformers recommends distinct query/document encoding for
  asymmetric semantic search and documents exact, chunked corpus scans for
  small corpora: <https://www.sbert.net/examples/sentence_transformer/applications/semantic-search/README.html>.
- LanceDB documents that updates create versions and that compaction does not
  reclaim disk until old versions are pruned, illustrating why a disposable,
  non-versioned sidecar is preferable at repo-link's scale:
  <https://docs.lancedb.com/tables/versioning> and
  <https://docs.lancedb.com/indexing/reindexing>.
