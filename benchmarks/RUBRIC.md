# AIC Commit Message Quality Rubric

Benchmark scoring rubric for AIC-14. Each dimension scores **0–5**; the total is
the sum across four dimensions (**0–20**). The scorer is deterministic
(`score.py`, no LLM in the loop) so scores are reproducible across runs and
variants; the reference (human-written) message from the sample's original
commit is archived alongside for future LLM-judge calibration.

Dimensions mirror the requirements in `SYSTEM_PROMPT_GIT_MESSAGE` plus what a
reader of the commit log actually needs.

## A. Conventional-commit compliance (0–5)

Checks the message parses as `<type>(<scope>): <subject>` per Conventional
Commits v1.0.0.

| Criterion | Points |
|---|---|
| Message parses as `<type>(<scope>): <subject>` | 2 |
| `type` ∈ {feat, fix, docs, style, refactor, test, chore, perf, ci, build, revert} | 1 |
| Subject ≤ 72 chars | 1 |
| Subject has no trailing period | 1 |

## B. Subject line conventions (0–5)

Checks the subject reads like a well-formed imperative commit subject.

| Criterion | Points |
|---|---|
| Subject starts with an imperative verb (checked against a known verb list) | 2 |
| Subject has ≥ 2 significant words (specificity — not just "update", "fix stuff") | 1 |
| No trailing punctuation / stray uppercase mid-subject | 1 |
| ≥ 1 significant subject token appears in the diff (ties subject to the change) | 1 |

## C. Body information content (0–5)

Checks the body says WHAT/WHY and stays lean. "Complex" diff = ≥ 3 files or
≥ 30 changed lines (matches prompt rule 5: omit body when self-explanatory).

| Criterion | Points |
|---|---|
| Complex diff → body present; simple diff → body absent | 2 |
| Body ≤ 300 chars (or absent) | 1 |
| Body contains WHY signal (keywords: because, avoid, allows, enables, prevents, fixes, needed, instead, so that, without) | 2 |

## D. Diff relevance (0–5)

Deterministic proxy for "does the message describe this diff": significant
tokens from subject + body must appear in the diff text.

| Criterion | Points |
|---|---|
| `5 × matched_significant_tokens / total_significant_tokens`, capped at 5 | 0–5 |

Tokens are lowercased, stripped of stopwords, and must be ≥ 4 chars. A token
"matches" if it appears in the diff's added/removed lines or hunk headers.

## Total

Sum of A+B+C+D, 0–20. Per-sample and per-variant means are reported by
`run_benchmark.py`; a variant "wins" when its mean total exceeds the baseline
by ≥ 0.5 with no dimension regression ≥ 0.5.
