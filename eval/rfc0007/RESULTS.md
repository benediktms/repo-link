# RFC 0007 — Stage 2 candidate-model evaluation

Result of the §10 retrieval-quality evaluation, run via the candle eval
driver (`crates/infra-embed/src/bin/embed_eval.rs`) against the labelled
fixture (`fixture.json`). The numbers below are the recorded results of that
run; the one-shot runner scripts that produced them were removed after the
decision (see the decision section), and the fixture + this document remain
as the reproducible record.

## Method

- Corpus: D2/D3 formatted chunks of all 44 fixture tasks (title-anchored core
  + comment chunks).
- Lanes per RFC 0007 D4: literal (raw-text Unicode fold), lexical
  (FTS5/BM25 over formatted chunks), semantic (exact cosine, best chunk per
  task), fused via RRF k=60. Identifier/exact-mode queries keep the
  literal-first hard guarantee.
- Metrics per category: Recall@10 and MRR for the semantic lane and the fused
  result. Recall@10 is the mean per-query recall, so a query with two relevant
  tasks scores 0.5 when the fused top-10 holds one of them.
- Gates (RFC 0007 §10):
  1. exact-match retention 100% — every exact query still has a relevant task
     at fused rank 1, i.e. fused MRR of 1.0 on the `exact` category. This is a
     per-query guarantee about the exact match itself, not aggregate Recall@10
     parity with the FTS baseline; see the note under the baselines table;
  2. fused results non-regressive vs the FTS-only baseline on every category;
  3. semantic lane beats the FTS-only paraphrase baseline by the predeclared
     margin (0.05).

## Baselines (no model)

| Category | n | FTS R@10 | FTS MRR | Fused R@10 | Fused MRR |
|---|---|---|---|---|---|
| exact | 21 | 0.976 | 0.976 | 0.952 | 1.0 |
| paraphrase | 30 | 0.8 | 0.619 | 0.8 | 0.619 |
| misleading | 15 | 0.594 | 0.783 | 0.594 | 0.75 |
| long_desc | 10 | 1.0 | 1.0 | 0.9 | 0.853 |
| length_bias | 10 | 0.8 | 0.723 | 0.7 | 0.717 |
| typo | 10 | 0.3 | 0.3 | 0.4 | 0.4 |
| language | 20 | 0.8 | 0.975 | 0.8 | 0.975 |
| closed | 10 | 1.0 | 1.0 | 1.0 | 1.0 |

Gate 1 baseline check: 0 exact-retention failures.

The `exact` row shows fused Recall@10 (0.952) below the FTS baseline (0.976)
while fused MRR rises to 1.0. Both follow from the fixture: 4 of the 21 exact
queries have two relevant tasks, and Recall@10 is the mean per-query recall, so
0.976 is 20.5/21 and 0.952 is 20.0/21 — a difference of exactly one secondary
relevant task falling past rank 10 for one multi-relevant query. The fused MRR
of 1.0 is the retention result that gate 1 measures: every exact query, without
exception, keeps a relevant task at rank 1, which the FTS baseline does not
(its MRR of 0.976 is one query whose first relevant task sat at rank 2).

## Candidates (semantic lane; fused in parentheses where it differs)

| Model | exact fR@10 | paraphrase sR@10 | gain vs FTS | misleading fR@10 | typo sR@10 | language sR@10 | size |
|---|---|---|---|---|---|---|---|
| bge-small-en-v1.5 | 0.976 | 0.933 | +0.133 | 0.8 | 0.6 | 0.825 | ~130 MB |
| e5-small-v2 | 0.976 | 0.933 | +0.133 | 0.811 | 0.75 | 0.9 | ~130 MB |
| **all-MiniLM-L6-v2** | **1.0** | **0.95** | **+0.15** | **0.894** | 0.65 | 0.8 | ~90 MB |
| multilingual-e5-small | 0.976 | 0.933 | +0.133 | 0.728 | 0.7 | 1.0 | ~470 MB |

All four candidates pass all three gates: gate 1 (0 exact-retention failures
in the fused result), gate 2 (no category regresses vs the FTS baseline), and
gate 3 (paraphrase gain 0.13–0.15, well above the 0.05 margin).

## Decision

**Pinned profile: `sentence-transformers/all-MiniLM-L6-v2`**
(revision `1110a243fdf4706b3f48f1d95db1a4f5529b4d41`).

Rationale:

- Best paraphrase gain (+0.15) — the entire justification for the semantic
  lane — and perfect exact retention (1.0 fused R@10).
- Strongest misleading-term handling (0.894 fused R@10), the category that
  penalises models which overfit surface vocabulary.
- Smallest footprint (~90 MB), which keeps the D7 model-cache budget and
  cold-load latency (Stage 4 gate) minimal.
- English-only is the current product scope. The typo weakness (0.65 vs 0.75
  for e5) is not product-relevant: typo'd input is a human behaviour, and the
  literal lane already covers identifier near-misses. If non-English task
  content becomes supported, `multilingual-e5-small` is the recorded upgrade
  path (1.0 on the language category).

Profile manifest (repo, revision, artifact digests, pooling, prefixes,
dimensions, max input) is compiled into the binary in
`crates/infra-embed/src/profiles.rs`; `embedding_profile_id` is the SHA-256 of
the canonical manifest.

## Reproduce

The fixture is checked in and regenerable:

```sh
python3 generate_fixture.py          # fixture.json (deterministic)
```

Re-running the full evaluation requires rebuilding the one-shot harness
(baseline + candidate runners) from the method description above; the
recorded results are the reference for the gates.
