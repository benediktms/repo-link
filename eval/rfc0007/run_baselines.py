#!/usr/bin/env python3
"""RFC 0007 Stage 2 literal + FTS/BM25 baselines on the fixture.

Computes the exact/lexical baseline the §10 gates compare against before any
model: literal (Unicode case-fold substring over raw text), lexical
(FTS5/BM25 over formatted chunks, D2/D3), fused (RRF k=60, D4). Writes
baselines.json + baselines.md. Stdlib SQLite only; no model, no network.
"""

from __future__ import annotations

import json
import re
import sqlite3
from pathlib import Path

HERE = Path(__file__).parent
FIXTURE = HERE / "fixture.json"
OUT = HERE / "baselines.json"
MD = HERE / "baselines.md"

RRF_K = 60.0
BODY_BUDGET = 918
TITLE_LABEL = "Title:"
DESC_LABEL = "Description:"
COMMENT_LABEL = "Comment:"

_IDENT_RE = re.compile(r"[_/]|-|\d+[A-Za-z]|::|[A-Z]{2,}|[a-z]+[A-Z]")


def _paragraphs(text: str) -> list[str]:
    return [p.strip() for p in text.split("\n") if p.strip()]


def _split_paragraph(para: str, budget: int) -> list[str]:
    if len(para.encode()) <= budget:
        return [para]
    sentences = re.split(r"(?<=[.!?])\s+", para)
    parts: list[str] = []
    cur = ""
    for s in sentences:
        if len((cur + " " + s).strip().encode()) <= budget:
            cur = (cur + " " + s).strip()
        else:
            if cur:
                parts.append(cur)
            if len(s.encode()) <= budget:
                cur = s
            else:
                b = s.encode()
                chunk = b[:budget]
                while chunk and (chunk[-1] & 0xC0) == 0x80:
                    chunk = chunk[:-1]
                if chunk and (chunk[-1] & 0xC0) == 0x80:
                    chunk = chunk[:-1]
                parts.append(chunk.decode())
                cur = b[budget:].decode(errors="ignore")
    if cur:
        parts.append(cur)
    return parts or [para]


def _title_anchor(title: str, budget: int) -> str:
    if len(title.encode()) <= budget:
        return title
    b = title.encode()
    chunk = b[: max(1, budget - 3)]
    while chunk and (chunk[-1] & 0xC0) == 0x80:
        chunk = chunk[:-1]
    if chunk and (chunk[-1] & 0xC0) == 0x80:
        chunk = chunk[:-1]
    return chunk.decode() + "…"


def format_core(task: dict) -> list[str]:
    title = task["title"]
    body = task["body"]
    head = f"{TITLE_LABEL} {title}\n\n{DESC_LABEL}\n"
    if not body:
        return [head.strip()]
    anchor = _title_anchor(title, BODY_BUDGET)
    chunks: list[str] = []
    paras = _paragraphs(body)
    for p in paras:
        for sub in _split_paragraph(p, BODY_BUDGET - len(anchor.encode()) - 4):
            chunks.append(f"{TITLE_LABEL} {anchor}\n\n{DESC_LABEL}\n{sub}")
    return chunks or [head.strip()]


def format_comment(task: dict, body: str) -> list[str]:
    anchor = _title_anchor(task["title"], BODY_BUDGET)
    chunks: list[str] = []
    for sub in _split_paragraph(str(body), BODY_BUDGET - len(anchor.encode()) - 4):
        chunks.append(f"{TITLE_LABEL} {anchor}\n\n{COMMENT_LABEL}\n{sub}")
    return chunks or [f"{TITLE_LABEL} {anchor}\n\n{COMMENT_LABEL}\n{body}"]


def build_chunks(tasks: list[dict]) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    for t in tasks:
        chunks = format_core(t)
        for c in t.get("comments") or []:
            chunks += format_comment(t, c)
        seen: set[str] = set()
        deduped = []
        for c in chunks:
            if c not in seen:
                seen.add(c)
                deduped.append(c)
        out[t["id"]] = deduped
    return out


def _fold(s: str) -> str:
    return s.casefold()


def literal_match(task: dict, needle: str, identifier_mode: bool) -> bool:
    raw = " ".join([task["title"], task["body"]] + [c for c in task.get("comments") or []])
    folded = _fold(raw)
    q = _fold(needle)
    if q in folded:
        return True
    if identifier_mode:
        # D4: only identifier-shaped tokens are lone needles; plain words are
        # covered by the full-query match and the lexical lane.
        for tok in re.findall(r"[A-Za-z0-9_-]+", needle):
            if not _IDENT_RE.search(tok):
                continue
            if _fold(tok) and _fold(tok) in folded:
                return True
    return False


def query_mode(query: str) -> str:
    if any(_IDENT_RE.search(tok) for tok in query.split()):
        return "identifier"
    return "natural"


def run_lexical(conn, tasks: list[dict], chunks: dict[str, list[str]],
                query: str) -> dict[str, float]:
    terms = [t for t in re.findall(r"[^\s]+", query)]
    match_expr = " OR ".join('"' + t.replace('"', '""') + '"' for t in terms)
    rows = conn.execute(
        "SELECT text FROM fts WHERE fts MATCH ? ORDER BY rank",
        (match_expr,),
    ).fetchall()
    best: dict[str, float] = {}
    for (text,) in rows:
        tid = CHUNK_TASK.get(text)
        if tid is None:
            continue
        rank = rows.index((text,))
        if tid not in best or rank < best[tid]:
            best[tid] = rank
    return best


CHUNK_TASK: dict[str, str] = {}


def rrf(rankings: list[dict[str, float]]) -> dict[str, float]:
    scores: dict[str, float] = {}
    for r in rankings:
        for i, tid in enumerate(sorted(r, key=lambda t: r[t])):
            scores[tid] = scores.get(tid, 0.0) + 1.0 / (RRF_K + i + 1)
    return scores


def recall_at_k(ranked: list[str], relevant: set[str], k: int) -> float:
    if not relevant:
        return 0.0
    return len(set(ranked[:k]) & relevant) / len(relevant)


def mrr(ranked: list[str], relevant: set[str]) -> float:
    for i, tid in enumerate(ranked):
        if tid in relevant:
            return 1.0 / (i + 1)
    return 0.0


def evaluate(tasks: list[dict], chunks: dict[str, list[str]], conn,
             queries: list[dict]) -> dict:
    CHUNK_TASK.clear()
    for tid, cs in chunks.items():
        for c in cs:
            CHUNK_TASK[c] = tid

    per_cat: dict[str, dict] = {}
    exact_retention_fail = []
    for q in queries:
        cat = q["category"]
        rel = set(q["relevant"])
        mode = query_mode(q["text"])

        lit_tasks = [
            t["id"] for t in tasks
            if literal_match(t, q["text"], mode == "identifier")
        ]
        lex = run_lexical(conn, tasks, chunks, q["text"])
        lex_ranked = sorted(lex, key=lambda t: lex[t])

        if mode == "natural":
            lit_rank = {t: i for i, t in enumerate(sorted(lit_tasks))}
            fused = rrf([lit_rank, lex])
        else:
            fused = rrf([lex])
            preceded = [t for t in lit_tasks] + [t for t in sorted(fused, key=fused.get)]
            seen: set[str] = set()
            order = []
            for t in preceded:
                if t not in seen:
                    seen.add(t)
                    order.append(t)
            fused = {t: -i for i, t in enumerate(order)}

        fused_ranked = sorted(fused, key=lambda t: (-fused.get(t, 0.0), t))

        if cat == "exact" and rel:
            top = fused_ranked[0] if fused_ranked else None
            if top not in rel:
                exact_retention_fail.append(q["text"])

        entry = per_cat.setdefault(cat, {
            "count": 0, "r10_lit": 0.0, "mrr_lit": 0.0,
            "r10_fused": 0.0, "mrr_fused": 0.0, "r10_fts": 0.0, "mrr_fts": 0.0,
        })
        entry["count"] += 1
        entry["r10_lit"] += recall_at_k([t for t in lit_tasks], rel, 10)
        entry["mrr_lit"] += mrr([t for t in lit_tasks], rel)
        entry["r10_fused"] += recall_at_k(fused_ranked, rel, 10)
        entry["mrr_fused"] += mrr(fused_ranked, rel)
        entry["r10_fts"] += recall_at_k(lex_ranked, rel, 10)
        entry["mrr_fts"] += mrr(lex_ranked, rel)

    cats: dict = {}
    for cat, e in per_cat.items():
        n = e["count"]
        cats[cat] = {
            "count": n,
            "literal_r10": round(e["r10_lit"] / n, 3),
            "literal_mrr": round(e["mrr_lit"] / n, 3),
            "fts_r10": round(e["r10_fts"] / n, 3),
            "fts_mrr": round(e["mrr_fts"] / n, 3),
            "fused_r10": round(e["r10_fused"] / n, 3),
            "fused_mrr": round(e["mrr_fused"] / n, 3),
        }
    return {
        "gate1_exact_retention_failures": exact_retention_fail,
        "categories": cats,
    }


def main() -> None:
    data = json.loads(FIXTURE.read_text())
    tasks = data["tasks"]
    queries = data["queries"]
    chunks = build_chunks(tasks)

    conn = sqlite3.connect(":memory:")
    conn.execute(
        "CREATE VIRTUAL TABLE fts USING fts5(text, tokenize='unicode61')"
    )
    conn.executemany("INSERT INTO fts(text) VALUES (?)",
                     [(c,) for cs in chunks.values() for c in cs])
    conn.commit()

    results = evaluate(tasks, chunks, conn, queries)

    OUT.write_text(json.dumps(results, indent=2) + "\n")
    lines = [
        "# RFC 0007 — Stage 2 baselines (literal + FTS/BM25)",
        "",
        f"Query count: {len(queries)} · Task count: {len(tasks)}",
        "",
        "| Category | n | Lit R@10 | Lit MRR | FTS R@10 | FTS MRR | Fused R@10 | Fused MRR |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for cat, e in results["categories"].items():
        lines.append(
            f"| {cat} | {e['count']} | {e['literal_r10']} | {e['literal_mrr']} "
            f"| {e['fts_r10']} | {e['fts_mrr']} | {e['fused_r10']} | {e['fused_mrr']} |"
        )
    g1 = results["gate1_exact_retention_failures"]
    lines.append("")
    lines.append(f"Gate 1 exact-retention failures: {len(g1)}")
    for f in g1:
        lines.append(f"  - {f}")
    MD.write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
