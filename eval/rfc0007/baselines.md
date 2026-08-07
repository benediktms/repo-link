# RFC 0007 — Stage 2 baselines (literal + FTS/BM25)

Query count: 126 · Task count: 44

| Category | n | Lit R@10 | Lit MRR | FTS R@10 | FTS MRR | Fused R@10 | Fused MRR |
|---|---|---|---|---|---|---|---|
| exact | 21 | 0.738 | 0.81 | 0.976 | 0.976 | 0.952 | 1.0 |
| paraphrase | 30 | 0.0 | 0.0 | 0.8 | 0.619 | 0.8 | 0.619 |
| misleading | 15 | 0.628 | 0.722 | 0.594 | 0.783 | 0.594 | 0.75 |
| long_desc | 10 | 0.2 | 0.15 | 1.0 | 1.0 | 0.9 | 0.853 |
| length_bias | 10 | 0.1 | 0.1 | 0.8 | 0.723 | 0.7 | 0.717 |
| typo | 10 | 0.2 | 0.2 | 0.3 | 0.3 | 0.4 | 0.4 |
| language | 20 | 0.25 | 0.4 | 0.8 | 0.975 | 0.8 | 0.975 |
| closed | 10 | 0.4 | 0.4 | 1.0 | 1.0 | 1.0 | 1.0 |

Gate 1 exact-retention failures: 0
