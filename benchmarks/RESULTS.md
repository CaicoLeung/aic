# AIC-14 Commit Message Benchmark — Results

Run date: 2026-08-07 · Model: `deepseek-v4-flash` (temperature 0.2) · 35 paired samples

## Verdict

**No variant beats the baseline.** Mean totals (0–20) on the 35-sample paired set:

| Variant | Mean | A conventional | B subject | C body | D relevance |
|---|---|---|---|---|---|
| **baseline** (shipped prompt) | **15.65** | 4.00 | 4.54 | 3.86 | 3.25 |
| variant-whatwhy (body/why emphasis) | 15.65 | 4.00 | 4.63 | 3.94 | 3.08 |
| variant-concrete (subject specificity) | 15.64 | 4.00 | 4.43 | 4.06 | 3.15 |

Adoption rule (RUBRIC.md): a variant wins only when its mean total beats the
baseline by ≥ 0.5 with no dimension regression ≥ 0.5. Neither variant qualifies
(max gain +0.00; both regress on dimension D relevance).

**Recommendation: keep the shipped prompt as default.** Do NOT change
`SYSTEM_PROMPT_GIT_MESSAGE`. The benchmark now guards future prompt edits.

## Category breakdown (mean total)

| Category | n | baseline | whatwhy | concrete |
|---|---|---|---|---|
| docs | 2 | 12.11 | 11.72 | 12.07 |
| feature | 8 | 15.66 | 15.37 | 15.27 |
| fix | 8 | 17.00 | **17.29** | 16.93 |
| multi-file | 8 | 15.56 | 15.39 | 15.26 |
| refactor | 7 | 15.21 | 15.18 | **15.81** |
| tests | 2 | 15.60 | **16.78** | 16.40 |

Signals for future iteration:
- **docs diffs are the weak spot** for every prompt (~12/20). The model
  under-specifies subject scope and omits why-bodies on doc-only diffs.
- variant-concrete's refactor gain (+0.60) and variant-whatwhy's tests/fix
  gains suggest a **hybrid** (concrete-subject rule + mandatory why-body for
  complex diffs) could win on a dedicated refactor/docs-focused sample set —
  before re-testing on the full set.
- Dimension D (diff relevance) is the lowest dimension for all variants;
  subjects drift from the actual diff. A future variant should force the
  subject's object noun to be a symbol/path present in the diff.

## Reproduce

```bash
python3 benchmarks/export_prompt.py        # sync prompt-baseline.txt from src/prompt.rs
python3 benchmarks/run_benchmark.py --variant baseline
python3 benchmarks/run_benchmark.py --variant variant-whatwhy
python3 benchmarks/run_benchmark.py --variant variant-concrete
python3 benchmarks/run_benchmark.py --variant <name> --score-only   # offline rescore
```

Raw generations: `results/<variant>.json` (cached; reruns skip completed
samples). Scoring is deterministic (`score.py`, rubric in `RUBRIC.md`).

## Environment notes

- Sample set: 36 real commits from aic's own history, quota-balanced across
  multi-file / refactor / fix / feature / tests / docs (see `samples.jsonl`).
- Each call retries empty-content up to 3× (DeepSeek reasoning models burn
  their budget on `reasoning_content` — same failure the production retry
  module solves).
- One baseline sample (s34) failed after 3 empty-content retries; excluded
  from paired stats.
