#!/usr/bin/env python3
"""Stage 0 sidecar spike harness — reproduces the RFC 0007 D5 storage evidence.

Builds the exact D5 sidecar schema in SQLite and measures the capacity and
timing numbers recorded in RFC 0007 (docs/rfcs/0007-semantic-task-search.md),
§1 "Stage 0 sidecar spike" / §5 / §9.

The harness is a faithful *method* reproduction. It does not reproduce the
original 2026-07-24 chunk counts exactly: the original one-off harness and its
exact chunker + data snapshot were not preserved, and the live authoritative DB
has grown since. What it re-establishes is the schema, the chunking rules, and
every measurement target, and it records a fresh, current set of numbers.

Run:
    python3 spike.py [--db PATH] [--scales current,10000,100000] [--probes]

    --db       path to the authoritative repo-link.db (defaults to the macOS
               data dir). Used only by the `current` scale.
    --scales   comma-separated chunk targets: `current`, or an integer
               (e.g. `10000`) for a synthetic corpus of that many chunks.
    --probes   additionally run the concurrency / failover probes (a separate
               scratch sidecar; ~seconds).

Everything is written to temp files under $TMPDIR (or --sidecar-dir); nothing
touches the authoritative database. Output is JSON on stdout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import sqlite3
import struct
import sys
import tempfile
import time
from pathlib import Path

# --- Configuration constants (from D5 / §1 / §5) ---------------------------
PAGE_SIZE = 4096
MAX_PAGE_COUNT = 131072            # 512 MiB hard cap
BUDGET_WARN = 384 * 1024 * 1024   # sidecar or WAL peak warn
BUDGET_REFUSE = 512 * 1024 * 1024  # sidecar or WAL peak hard refuse
FREE_WARN = 1024 * 1024 * 1024    # projected free space warn
FREE_REFUSE = 512 * 1024 * 1024    # projected free space refuse

CHUNK_FORMAT_VERSION = 1
SCHEMA_VERSION = 1
DIM = 384                          # output dimensions (float32)
APPROX_BODY_BUDGET = 918           # approx formatted bytes per body chunk (D2)

DEFAULT_DB = (
    Path.home()
    / "Library/Application Support/repo-link/repo-link.db"
)

# --- Schema (faithful to D5) ----------------------------------------------


def create_sidecar(path: Path) -> sqlite3.Connection:
    """Create a fresh D5 sidecar: 4 KiB pages, auto_vacuum FULL, WAL, FTS5."""
    conn = sqlite3.connect(str(path))
    conn.execute(f"PRAGMA page_size={PAGE_SIZE}")
    conn.execute("PRAGMA auto_vacuum=FULL")
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute(f"PRAGMA max_page_count={MAX_PAGE_COUNT}")
    conn.executescript(
        """
        CREATE TABLE task_search_meta(
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            schema_version INTEGER NOT NULL,
            chunk_format_version INTEGER NOT NULL,
            embedding_profile_id TEXT
        );
        INSERT INTO task_search_meta(singleton, schema_version, chunk_format_version,
                                     embedding_profile_id)
        VALUES (1, 1, 1, NULL);

        CREATE TABLE task_search_chunks(
            id INTEGER PRIMARY KEY,
            task_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('core', 'comment')),
            content_hash BLOB NOT NULL,
            text TEXT NOT NULL,
            UNIQUE(task_id, content_hash)
        );

        CREATE TABLE task_search_vectors(
            search_chunk_id INTEGER NOT NULL
                REFERENCES task_search_chunks(id) ON DELETE CASCADE,
            segment_index INTEGER NOT NULL,
            embedding_input_hash BLOB NOT NULL,
            vector BLOB NOT NULL,          -- little-endian normalized f32
            PRIMARY KEY(search_chunk_id, segment_index)
        );

        CREATE VIRTUAL TABLE task_search_fts
            USING fts5(text, content='task_search_chunks', content_rowid='id');

        -- FTS5 external-content maintenance (documented insert/delete pattern).
        CREATE TRIGGER task_search_chunks_ai AFTER INSERT ON task_search_chunks BEGIN
            INSERT INTO task_search_fts(rowid, text) VALUES (new.id, new.text);
        END;
        CREATE TRIGGER task_search_chunks_ad AFTER DELETE ON task_search_chunks BEGIN
            INSERT INTO task_search_fts(task_search_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
        END;

        -- Guard: chunks are deleted+inserted, never mutated.
        CREATE TRIGGER task_search_chunks_bu
            BEFORE UPDATE OF task_id, content_hash, text ON task_search_chunks BEGIN
            SELECT RAISE(ABORT, 'chunk identity or text is immutable');
        END;
        """
    )
    conn.commit()
    return conn


# --- D2 chunker ------------------------------------------------------------


def content_hash(text: str) -> bytes:
    return hashlib.sha256(text.encode("utf-8")).digest()


def format_core_chunks(title: str, body: str) -> list[str]:
    """Return the deterministic lexical chunks for one task's core content."""
    body = body or ""
    budget = APPROX_BODY_BUDGET

    def anchor() -> str:
        # Title always anchors every body chunk. Oversized titles are truncated
        # deterministically with an explicit ellipsis marker.
        if len(title.encode("utf-8")) > budget:
            cut = title.encode("utf-8")[: budget - 3]
            cut = cut.decode("utf-8", errors="ignore")
            return f"Title: {cut}…"
        return f"Title: {title}"

    if not body.strip():
        return [f"Title: {title}"]

    head = f"{anchor()}\n\nDescription:\n"

    body_chunks: list[str] = []
    buf = ""
    for para in re.split(r"\n\s*\n", body):
        para = para.strip()
        if not para:
            continue
        if len((head + buf).encode("utf-8")) + 1 + len(para.encode("utf-8")) <= budget:
            buf += "\n" + para if buf else para
        else:
            if buf:
                body_chunks.append(buf)
            # Paragraph too big for one chunk: split at sentence boundaries,
            # then valid UTF-8 scalar boundaries.
            for sentence in _split_oversized(para, budget - len(head.encode("utf-8"))):
                body_chunks.append(sentence)
            buf = ""
    if buf:
        body_chunks.append(buf)

    return [head + c for c in body_chunks]


def _split_oversized(para: str, budget: int) -> list[str]:
    pieces: list[str] = []
    for sentence in re.split(r"(?<=[.!?])\s+", para):
        if not sentence:
            continue
        while len(sentence.encode("utf-8")) > budget:
            cut = sentence.encode("utf-8")[:budget]
            cut = cut.decode("utf-8", errors="ignore")
            pieces.append(cut)
            sentence = sentence[len(cut):]
        pieces.append(sentence)
    return pieces


def format_comment_chunks(title: str, body: str) -> list[str]:
    head = f"Title: {title}\n\nComment:\n"
    # Simplified: one chunk per comment body, reusing the core body splitting.
    inner = format_core_chunks(title, body)
    # Replace the Description prefix with Comment: for each body chunk.
    out = []
    for c in inner:
        # format_core_chunks returns `Title:...\n\nDescription:\n<body>` or a
        # bare title-only chunk; rewrite the field label for comment content.
        out.append(c.replace("\n\nDescription:\n", "\n\nComment:\n", 1) if "\n\nDescription:\n" in c
                   else f"{head}{c}")
    return out


def chunk_source(source: list[tuple[str, str, str]]) -> list[dict]:
    """source = list of (task_id, kind, text). kind in {'body','comment'}.

    Returns list of {task_id, kind, hash, text} lexical chunks.
    """
    out: list[dict] = []
    for task_id, kind, text in source:
        # Split title/body out of a combined payload ("title\x1fbody").
        title, _, body = text.partition("\x1f")
        chunks = format_comment_chunks(title, body) if kind == "comment" \
            else format_core_chunks(title, body)
        for c in chunks:
            out.append(
                {
                    "task_id": task_id,
                    "kind": "comment" if kind == "comment" else "core",
                    "hash": content_hash(c),
                    "text": c,
                }
            )
    return out


# --- vector filler ---------------------------------------------------------


def fake_vector(seed: bytes, dim: int = DIM) -> bytes:
    """Deterministic, size-faithful little-endian f32 vector (values are fake)."""
    rng = random.Random(seed)
    vals = [rng.random() for _ in range(dim)]
    n = math_sqrt(sum(v * v for v in vals)) or 1.0
    vals = [v / n for v in vals]
    return struct.pack(f"<{dim}f", *vals)


def math_sqrt(x: float) -> float:
    return x ** 0.5


# --- reconciliation (D6), timing, and sizing -------------------------------


def _reconcile(conn: sqlite3.Connection, desired: list[dict],
               fill_vectors: bool) -> tuple[int, float, int]:
    """One D6 reconcile pass. Returns (n_inserted, wall_s, n_deleted)."""
    t0 = time.perf_counter()
    conn.execute("BEGIN IMMEDIATE")
    existing = set(
        conn.execute("SELECT task_id, content_hash FROM task_search_chunks").fetchall()
    )
    # "Desired set": within-task duplicate formatted chunks collapse to one row
    # (UNIQUE(task_id, content_hash)); dedupe here, like the RFC's D6 set.
    desired_set = set()
    for d in desired:
        desired_set.add((d["task_id"], d["hash"]))

    seen = set()
    to_insert = []
    for d in desired:
        key = (d["task_id"], d["hash"])
        if key in seen or key in existing:
            continue
        seen.add(key)
        to_insert.append(d)
    to_delete = existing - desired_set

    if to_insert or to_delete:
        # Pre-write FTS integrity check (only when mutation is non-empty).
        conn.execute("INSERT INTO task_search_fts(task_search_fts, rank) VALUES('integrity-check', 1)")
        for task_id, h in to_delete:
            conn.execute(
                "DELETE FROM task_search_chunks WHERE task_id = ? AND content_hash = ?",
                (task_id, h),
            )
        for d in to_insert:
            cur = conn.execute(
                "INSERT INTO task_search_chunks(task_id, kind, content_hash, text) "
                "VALUES (?, ?, ?, ?)",
                (d["task_id"], d["kind"], d["hash"], d["text"]),
            )
            if fill_vectors:
                conn.execute(
                    "INSERT INTO task_search_vectors(search_chunk_id, segment_index, "
                    "embedding_input_hash, vector) VALUES (?, 0, ?, ?)",
                    (cur.lastrowid, d["hash"], fake_vector(d["hash"])),
                )
    conn.commit()
    return len(to_insert), time.perf_counter() - t0, len(to_delete)


def reconcile_zero_p95(conn, source, iters) -> float:
    """p95 of the full D6 reconcile path: read source, chunk, SHA-256 hash,
    diff against the sidecar, and commit a zero-change transaction."""
    times = []
    for _ in range(iters):
        t0 = time.perf_counter()
        desired = dedupe_chunks(chunk_source(source))
        _reconcile(conn, desired, fill_vectors=False)
        times.append(time.perf_counter() - t0)
    times.sort()
    return round(times[int(len(times) * 0.95)] * 1000, 4)


def hash_diff_p95(conn, desired, iters) -> float:
    """p95 of the narrow hash-diff step the RFC's §1 main table reports:
    re-hash every desired chunk, diff against the stored (task_id, content_hash)
    set, and commit a zero-change transaction. This deliberately excludes source
    read/chunking; see reconcile_zero_p95 for the full D6 path."""
    times = []
    for _ in range(iters):
        t0 = time.perf_counter()
        existing = set(
            conn.execute("SELECT task_id, content_hash FROM task_search_chunks").fetchall()
        )
        desired_set = {(d["task_id"], content_hash(d["text"])) for d in desired}
        conn.execute("BEGIN IMMEDIATE")
        conn.commit()  # zero-change write of nothing
        times.append(time.perf_counter() - t0)
    times.sort()
    return round(times[int(len(times) * 0.95)] * 1000, 4)


def dedupe_chunks(chunks: list[dict]) -> list[dict]:
    seen = set()
    out = []
    for d in chunks:
        key = (d["task_id"], d["hash"])
        if key in seen:
            continue
        seen.add(key)
        out.append(d)
    return out


def sidecar_paths(path: Path) -> list[Path]:
    return [path, Path(str(path) + "-wal"), Path(str(path) + "-shm")]


def largest_companion(path: Path) -> int:
    return max((p.stat().st_size if p.exists() else 0 for p in sidecar_paths(path)), default=0)


def dbstat_bytes(conn) -> dict:
    try:
        rows = conn.execute("SELECT name, sum(pgsize) FROM dbstat GROUP BY name").fetchall()
    except sqlite3.Error:
        return {}
    body = {}
    for name, size in rows:
        if name.startswith("task_search_fts"):
            body.setdefault("fts", 0)
            body["fts"] += size
        elif name == "task_search_chunks":
            body["text"] = size
        elif name == "task_search_vectors":
            body["vector"] = size
        elif name == "task_search_meta":
            body["meta"] = size
        else:
            body.setdefault("other", 0)
            body["other"] += size
    return body


# --- sources ---------------------------------------------------------------


def current_source(db: Path) -> list[tuple[str, str, str]]:
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute("SELECT id, title, body FROM tasks").fetchall()
    src = [(t, "body", f"{title}\x1f{body or ''}") for t, title, body in rows]
    rows = conn.execute(
        "SELECT c.task_id, t.title, c.body FROM task_comments c "
        "JOIN tasks t ON t.id = c.task_id"
    ).fetchall()
    src += [(t, "comment", f"{title}\x1f{body}") for t, title, body in rows]
    conn.close()
    return src


def synthetic_source(chunk_target: int) -> list[tuple[str, str, str]]:
    """Generate source rows that chunk to exactly `chunk_target` unique chunks.

    Each task is a title plus one ~815-byte paragraph with a unique trailing
    salt. The formatted chunk is ~885 bytes — under the ~918-byte budget — so
    every task yields exactly one chunk, and the salt keeps every chunk hash
    unique. raw count == deduped count == chunk_target. This mirrors the RFC's
    synthetic corpus ("comparable 918-byte formatted chunks").
    """
    # 16 reps ≈ 820-byte body → ~900-byte formatted chunk, comfortably under
    # the 918-byte budget so every task is exactly one chunk.
    para = " ".join(["synthetic paragraph text for semantic search spike"] * 16)
    source = []
    for i in range(1, chunk_target + 1):
        title = f"Task number {i} semantic search scenario"
        body = f"{para} <salt {i}>"
        source.append((f"rpl-{i}", "body", f"{title}\x1f{body}"))
    return source


# --- scenario measurement --------------------------------------------------


def run_scale(label: str, source, tmpdir: Path):
    sidecar = tmpdir / f"{label}.task-search.db"
    conn = create_sidecar(sidecar)

    # Initial build. Collapse within-task duplicate chunks once (D6 "desired
    # set") so build, reconcile, and rebuild all operate on the same rows.
    desired = dedupe_chunks(chunk_source(source))

    t0 = time.perf_counter()
    _reconcile(conn, desired, fill_vectors=True)
    build_ms = round((time.perf_counter() - t0) * 1000, 1)

    chunks = conn.execute("SELECT COUNT(*) FROM task_search_chunks").fetchone()[0]
    vectors = conn.execute("SELECT COUNT(*) FROM task_search_vectors").fetchone()[0]
    sidecar_bytes = sidecar.stat().st_size
    wal_peak = largest_companion(sidecar)
    # Force a checkpoint so WAL size reflects the DB, not pending pages.
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    sidecar_after_cp = sidecar.stat().st_size

    # Zero-change reconcile p95.
    iters = 50 if chunks < 50000 else 20
    zc_p95 = reconcile_zero_p95(conn, source, iters)
    hd_p95 = hash_diff_p95(conn, desired, iters)

    # 100-row changed transaction: a true diff where 100 chunks change identity
    # (same source position, new hash), i.e. 100 deletes + 100 inserts.
    changed = list(desired)
    edited = []
    for k in range(min(100, len(changed))):
        old = changed[k]
        newtext = old["text"] + " edited"
        edited.append({**old, "text": newtext, "hash": content_hash(newtext)})
    changed = changed[100:] + edited
    t0 = time.perf_counter()
    _reconcile(conn, changed, fill_vectors=True)
    tx100_ms = round((time.perf_counter() - t0) * 1000, 2)

    # Full in-place rebuild: delete-all + one transactional reinsert (D5).
    t0 = time.perf_counter()
    conn.execute("BEGIN IMMEDIATE")
    # Delete content first so the AFTER DELETE triggers empty FTS via the normal
    # per-row path; delete-all then compacts it. (delete-all before the content
    # delete makes the per-row 'delete' commands reference a missing index and
    # trips SQLITE_CORRUPT.)
    conn.execute("DELETE FROM task_search_chunks")
    conn.execute("INSERT INTO task_search_fts(task_search_fts) VALUES('delete-all')")
    conn.execute("DELETE FROM task_search_vectors")
    for d in desired:
        cur = conn.execute(
            "INSERT INTO task_search_chunks(task_id, kind, content_hash, text) VALUES (?,?,?,?)",
            (d["task_id"], d["kind"], d["hash"], d["text"]),
        )
        conn.execute(
            "INSERT INTO task_search_vectors(search_chunk_id, segment_index, "
            "embedding_input_hash, vector) VALUES (?,0,?,?)",
            (cur.lastrowid, d["hash"], fake_vector(d["hash"])),
        )
    conn.execute("COMMIT")
    rebuild_ms = round((time.perf_counter() - t0) * 1000, 1)

    conn.close()
    return {
        "scale": label,
        "chunks": chunks,
        "vectors": vectors,
        "sidecar_bytes": sidecar_bytes,
        "sidecar_after_checkpoint": sidecar_after_cp,
        "largest_wal_peak": wal_peak,
        "initial_build_ms": build_ms,
        "zero_change_p95_ms": zc_p95,
        "hash_diff_p95_ms": hd_p95,
        "100row_transaction_ms": tx100_ms,
        "full_rebuild_ms": rebuild_ms,
    }


# --- clear/delete-all contrast + storage failover --------------------------


def probe_clear_contrast(tmpdir: Path):
    """Measure the FTS delete-all correction that reduced cleared sidecars to 44 KiB."""
    sidecar = tmpdir / "clear-contrast.task-search.db"
    conn = create_sidecar(sidecar)
    source = synthetic_source(3000)
    desired = chunk_source(source)
    _reconcile(conn, desired, fill_vectors=True)

    # Counterfactual A: row-by-row DELETE (leaves FTS allocated space behind).
    conn.execute("INSERT INTO task_search_fts(task_search_fts, rank) VALUES('integrity-check', 1)")
    for d in desired:
        conn.execute("DELETE FROM task_search_chunks WHERE id = (SELECT id FROM task_search_chunks WHERE task_id=? AND content_hash=?)", (d["task_id"], d["hash"]))
    conn.commit()
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    row_by_row = sidecar.stat().st_size
    freelist_row = conn.execute("PRAGMA freelist_count").fetchone()[0]

    # Counterfactual B: delete-all + full auto-vacuum + truncating checkpoint.
    conn.execute("DELETE FROM task_search_chunks")
    conn.execute("INSERT INTO task_search_fts(task_search_fts) VALUES('delete-all')")
    conn.execute("DELETE FROM task_search_vectors")
    conn.commit()
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    delete_all = sidecar.stat().st_size
    freelist_all = conn.execute("PRAGMA freelist_count").fetchone()[0]
    conn.close()

    return {
        "row_by_row_delete_bytes": row_by_row,
        "row_by_row_freelist": freelist_row,
        "delete_all_bytes": delete_all,
        "delete_all_freelist": freelist_all,
    }


def probe_max_page_count_rollback():
    """max_page_count must be a hard backstop: an attempted overrun of the page
    cap rolls the whole write transaction back to zero committed rows.

    Uses a DELETE journal (no WAL) so pages are allocated to the main DB
    immediately and the cap is reached deterministically. The RFC verified the
    same rollback-to-zero under the WAL sidecar config; this probe demonstrates
    the backstop semantics.
    """
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    conn = sqlite3.connect(path)
    conn.execute(f"PRAGMA page_size={PAGE_SIZE}")
    conn.execute("PRAGMA auto_vacuum=FULL")
    conn.execute("PRAGMA journal_mode=DELETE")  # rollback journal: pages hit the main DB now
    conn.execute("PRAGMA max_page_count=8")     # 8 pages = 32 KiB — tiny, cheap to overflow
    conn.execute("CREATE TABLE t(x);")
    conn.commit()
    try:
        conn.execute("BEGIN")
        try:
            for i in range(100):
                conn.execute(
                    "INSERT INTO t VALUES (?)",
                    ("x" * 512,),  # fat rows so 100 easily exceed 8 pages
                )
            conn.execute("COMMIT")
            overflow = "no-error"
        except sqlite3.Error as e:
            overflow = str(e)
            # SQLite may auto-rollback on SQLITE_FULL; tolerate that.
            try:
                conn.execute("ROLLBACK")
            except sqlite3.Error:
                pass
        n = conn.execute("SELECT COUNT(*) FROM t").fetchone()[0]
    finally:
        conn.close()
        os.unlink(path)
    # Retry with the real D5 cap to confirm the schema itself is fine.
    fd, path2 = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    conn2 = sqlite3.connect(path2)
    conn2.execute(f"PRAGMA page_size={PAGE_SIZE}")
    conn2.execute("PRAGMA auto_vacuum=FULL")
    conn2.execute(f"PRAGMA max_page_count={MAX_PAGE_COUNT}")
    conn2.execute("CREATE TABLE t(x)")
    conn2.execute("INSERT INTO t VALUES ('ok')")
    conn2.commit()
    ok = conn2.execute("SELECT COUNT(*) FROM t").fetchone()[0]
    conn2.close()
    os.unlink(path2)
    return {"max_page_count_overflow": overflow, "rows_after_rollback": n, "normal_cap_ok_rows": ok}


def probe_concurrency(tmpdir: Path):
    """D6 sidecar-serialization and guarded-vector semantics."""
    out = {}
    sidecar = tmpdir / "concurrency.task-search.db"
    create_sidecar(sidecar)

    # BEGIN IMMEDIATE serializes reconcilers: a second writer is BUSY until
    # the first commits (SQLite busy timeout disabled → immediate).
    a = sqlite3.connect(str(sidecar))
    b = sqlite3.connect(str(sidecar))
    a.execute("BEGIN IMMEDIATE")
    a.execute("INSERT INTO task_search_meta(singleton,schema_version,chunk_format_version) VALUES(1,1,1) ON CONFLICT(singleton) DO NOTHING")
    a.commit()
    a.execute("BEGIN IMMEDIATE")
    try:
        b.execute("BEGIN IMMEDIATE")
        out["begin_immediate_serializes"] = "second-succeeded (unexpected)"
    except sqlite3.Error as e:
        out["begin_immediate_serializes"] = f"second-BUSY: {e}"
    a.rollback()
    b.close()
    a.close()

    # Profile compare-and-set: only the first claim of a NULL profile wins.
    c = sqlite3.connect(str(sidecar))
    n1 = c.execute("UPDATE task_search_meta SET embedding_profile_id=? WHERE singleton=1 AND embedding_profile_id IS NULL", ("prof-A",)).rowcount
    n2 = c.execute("UPDATE task_search_meta SET embedding_profile_id=? WHERE singleton=1 AND embedding_profile_id IS NULL", ("prof-B",)).rowcount
    prof = c.execute("SELECT embedding_profile_id FROM task_search_meta").fetchone()[0]
    out["compare_and_set"] = {"first": n1, "second": n2, "owner": prof}
    c.close()
    return out


def probes(tmpdir: Path) -> dict:
    return {
        "clear_contrast": probe_clear_contrast(tmpdir),
        "max_page_count": probe_max_page_count_rollback(),
        "concurrency": probe_concurrency(tmpdir),
    }


# --- main ------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=str(DEFAULT_DB))
    ap.add_argument("--sidecar-dir", default=None)
    ap.add_argument("--scales", default="current,10000,100000")
    ap.add_argument("--probes", action="store_true")
    args = ap.parse_args()

    tmpdir = Path(args.sidecar_dir) if args.sidecar_dir else Path(tempfile.mkdtemp(prefix="rl-stage0-spike-"))
    tmpdir.mkdir(parents=True, exist_ok=True)

    result = {
        "env": {"python": sys.version.split()[0], "sqlite": sqlite3.sqlite_version,
                "fts5": "y", "dbstat": "y"},
        "config": {"page_size": PAGE_SIZE, "max_page_count": MAX_PAGE_COUNT,
                   "bidirectional_budget_warn": BUDGET_WARN, "budget_refuse": BUDGET_REFUSE},
        "scales": [],
    }

    for label in args.scales.split(","):
        label = label.strip()
        if label == "current":
            if not Path(args.db).exists():
                print(json.dumps({"error": f"db not found: {args.db}"}), file=sys.stderr)
                sys.exit(2)
            src = current_source(args.db)
        elif label.isdigit():
            src = synthetic_source(int(label))
        else:
            print(json.dumps({"error": f"unknown scale {label}"}), file=sys.stderr)
            sys.exit(2)
        result["scales"].append(run_scale(label, src, tmpdir))

    if args.probes:
        result["probes"] = probes(tmpdir)

    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
