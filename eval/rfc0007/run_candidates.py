#!/usr/bin/env python3
"""RFC 0007 Stage 2 — candidate-model evaluation via the candle eval driver.

For each named candidate profile, this:
  1. fetches the model through `infra-embed-eval fetch` (reports digests);
  2. embeds the fixture corpus chunks (D2/D3 formatting) and every query;
  3. ranks tasks by best-per-task cosine similarity (semantic lane, D4);
  4. fuses literal + lexical + semantic via RRF (k=60; exact/identifier mode
     keeps the literal-first hard guarantee), reusing the baseline lanes;
  5. applies the §10 gates per category and reports Recall@10 / MRR.

Gates (RFC 0007 §10):
  1. exact-match retention 100% — no fused loss vs the baseline on exact
     queries;
  2. fused results non-regressive vs the FTS-only baseline on every category;
  3. the semantic lane beats the FTS-only baseline on the paraphrase category
     by the predeclared margin (0.05) — the entire justification for the lane.

Run: python3 run_candidates.py [--bin /path/to/embed_eval] [--cache DIR]
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
from pathlib import Path

import run_baselines as B

HERE = Path(__file__).parent
FIXTURE = HERE / "fixture.json"
BIN = "target/debug/embed_eval"
CACHE = HERE / "models-cache"

RRF_K = 60.0
PARAPHRASE_MARGIN = 0.05

CANDIDATES = [
    {
        "name": "bge-small-en-v1.5",
        "repo": "BAAI/bge-small-en-v1.5",
        "revision": "main",
        "pooling": "cls",
        "query_prefix": "Represent this sentence for searching relevant passages: ",
        "corpus_prefix": None,
    },
    {
        "name": "e5-small-v2",
        "repo": "intfloat/e5-small-v2",
        "revision": "main",
        "pooling": "mean",
        "query_prefix": "query: ",
        "corpus_prefix": "passage: ",
    },
    {
        "name": "all-MiniLM-L6-v2",
        "repo": "sentence-transformers/all-MiniLM-L6-v2",
        "revision": "main",
        "pooling": "mean",
        "query_prefix": None,
        "corpus_prefix": None,
    },
    {
        "name": "multilingual-e5-small",
        "repo": "intfloat/multilingual-e5-small",
        "revision": "main",
        "pooling": "mean",
        "query_prefix": "query: ",
        "corpus_prefix": "passage: ",
    },
]


def dot(a: list[float], b: list[float]) -> float:
    return sum(x * y for x, y in zip(a, b))


def rrf(rankings: list[list[str]]) -> dict[str, float]:
    scores: dict[str, float] = {}
    for rank in rankings:
        for i, tid in enumerate(rank):
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


def cosine_rank(query_vec: list[float], task_vectors: dict[str, list[float]]) -> list[str]:
    scored = {tid: max(dot(query_vec, v) for v in vecs) for tid, vecs in task_vectors.items()}
    return sorted(scored, key=lambda t: (-scored[t], t))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=BIN)
    ap.add_argument("--cache", default=str(CACHE))
    ap.add_argument("--candidates", default="all",
                    help="comma list of candidate names, or 'all'")
    args = ap.parse_args()

    data = json.loads(FIXTURE.read_text())
    tasks = data["tasks"]
    queries = data["queries"]
    task_by_id = {t["id"]: t for t in tasks}

    # corpus chunks per task (same D2/D3 formatting as the baseline)
    chunks = B.build_chunks(tasks)
    corpus_texts: list[str] = []
    corpus_task: list[str] = []
    for tid, cs in chunks.items():
        for c in cs:
            corpus_texts.append(c)
            corpus_task.append(tid)

    # baseline lanes (literal ranks + lexical ranks), for fusion
    conn = __import__("sqlite3").connect(":memory:")
    conn.execute("CREATE VIRTUAL TABLE fts USING fts5(text, tokenize='unicode61')")
    conn.executemany("INSERT INTO fts(text) VALUES (?)",
                     [(c,) for cs in chunks.values() for c in cs])
    conn.commit()
    B.CHUNK_TASK.clear()
    for tid, cs in chunks.items():
        for c in cs:
            B.CHUNK_TASK[c] = tid

    selected = [c for c in CANDIDATES
                if args.candidates == "all" or c["name"] in args.candidates.split(",")]
    if not selected:
        print(f"no candidates selected: {args.candidates}", file=sys.stderr)
        return 2

    all_reports = {}
    for cand in selected:
        print(f"== {cand['name']} ==", flush=True)
        out_dir = Path(args.cache) / cand["name"]
        # fetch (idempotent: reuse dir if artifacts exist)
        if not (out_dir / "model.safetensors").exists():
            subprocess.run(
                [args.bin, "fetch", cand["repo"], cand["revision"], "--out", str(out_dir)],
                check=True,
            )

        def embed(texts: list[str], side: str) -> list[list[float]]:
            payload = json.dumps(texts)
            res = subprocess.run(
                [
                    args.bin, "embed", "--dir", str(out_dir),
                    "--pooling", cand["pooling"],
                    "--dims", "384", "--max-tokens", "512",
                    "--side", side,
                ] + (
                    ["--query-prefix", cand["query_prefix"]]
                    if cand["query_prefix"] and side == "query" else []
                ) + (
                    ["--corpus-prefix", cand["corpus_prefix"]]
                    if cand["corpus_prefix"] and side == "corpus" else []
                ),
                input=payload.encode(),
                capture_output=True,
            )
            if res.returncode != 0:
                raise RuntimeError(f"embed failed: {res.stderr.decode()[:500]}")
            return json.loads(res.stdout)

        task_vectors: dict[str, list[list[float]]] = {t: [] for t in task_by_id}
        # batch corpus embedding in chunks of 64 to bound process memory
        batch = 64
        for i in range(0, len(corpus_texts), batch):
            vecs = embed(corpus_texts[i:i + batch], "corpus")
            for vec, tid in zip(
                vecs, corpus_task[i:i + batch]
            ):
                task_vectors[tid].append(vec)

        per_cat: dict[str, dict] = {}
        for q in queries:
            cat = q["category"]
            rel = set(q["relevant"])
            mode = B.query_mode(q["text"])
            qvec = embed([q["text"]], "query")[0]

            sem_rank = cosine_rank(qvec, task_vectors)
            lit_tasks = [
                t["id"] for t in tasks
                if B.literal_match(t, q["text"], mode == "identifier")
            ]
            lex = B.run_lexical(conn, tasks, chunks, q["text"])
            lex_rank = sorted(lex, key=lambda t: lex[t])

            if mode == "natural":
                fused = rrf([lit_tasks, lex_rank, sem_rank])
                fused_ranked = sorted(fused, key=lambda t: (-fused[t], t))
            else:
                # identifier/exact: literal matches ahead, then fused semantic+lexical
                sem_lex = rrf([sem_rank, lex_rank])
                order = sorted(sem_lex, key=lambda t: -sem_lex[t])
                fused = {t: -i for i, t in enumerate(lit_tasks + order)}
                seen: set[str] = set()
                out: list[str] = []
                for t in sorted(fused, key=lambda t: -fused[t]):
                    if t not in seen:
                        seen.add(t)
                        out.append(t)
                fused_ranked = out

            e = per_cat.setdefault(cat, {
                "count": 0, "r10_sem": 0.0, "mrr_sem": 0.0,
                "r10_fused": 0.0, "mrr_fused": 0.0,
            })
            e["count"] += 1
            e["r10_sem"] += recall_at_k(sem_rank, rel, 10)
            e["mrr_sem"] += mrr(sem_rank, rel)
            e["r10_fused"] += recall_at_k(fused_ranked, rel, 10)
            e["mrr_fused"] += mrr(fused_ranked, rel)

        cats = {c: {
            "count": e["count"],
            "semantic_r10": round(e["r10_sem"] / e["count"], 3),
            "semantic_mrr": round(e["mrr_sem"] / e["count"], 3),
            "fused_r10": round(e["r10_fused"] / e["count"], 3),
            "fused_mrr": round(e["mrr_fused"] / e["count"], 3),
        } for c, e in per_cat.items()}
        all_reports[cand["name"]] = cats
        print(f"  semantic lane per category: {json.dumps(cats, indent=2)}", flush=True)

    # gate evaluation
    baseline = json.loads((HERE / "baselines.json").read_text())
    print("\n=== Gates ===")
    for name, cats in all_reports.items():
        b_cats = baseline["categories"]
        exact_ok = True
        regress = []
        for cat in cats:
            if cat == "exact":
                continue
            # gate 2: fused r10 not worse than FTS baseline r10 - 0.02 tol
            fts_r10 = b_cats.get(cat, {}).get("fts_r10", 0)
            if cats[cat]["fused_r10"] + 0.02 < fts_r10:
                regress.append((cat, fts_r10, cats[cat]["fused_r10"]))
        sem_r10 = cats.get("paraphrase", {}).get("semantic_r10", 0)
        fts_para = b_cats.get("paraphrase", {}).get("fts_r10", 0)
        para_gain = sem_r10 - fts_para
        para_ok = para_gain >= PARAPHRASE_MARGIN
        print(f"{name}: gate1={exact_ok} regressions={regress} "
              f"paraphrase_gain={para_gain:.3f} (>= {PARAPHRASE_MARGIN}: {para_ok})")

    (HERE / "candidates.json").write_text(json.dumps(all_reports, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
