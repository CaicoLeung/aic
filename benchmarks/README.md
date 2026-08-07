# AIC commit-message benchmark

Regression harness for the commit-message prompt (`SYSTEM_PROMPT_GIT_MESSAGE`
in `src/prompt.rs`). Outcome of AIC-14: no prompt variant beat the baseline;
the harness now guards future prompt edits.

| File | Purpose |
|---|---|
| `extract_samples.py` | Rebuild `samples.jsonl` from aic's git history (quota-balanced) |
| `samples.jsonl` | 36 real diff samples + reference human commit messages |
| `RUBRIC.md` | Scoring rubric (4 dimensions × 0–5 = 0–20) |
| `score.py` | Deterministic scorer (no LLM, reproducible) |
| `run_benchmark.py` | LLM runner: cached, bounded, retries empty responses |
| `export_prompt.py` | Sync `prompt-baseline.txt` from `src/prompt.rs` |
| `prompts/` | Prompt variants under test |
| `results/` | Raw generations (`<variant>.json`) + summaries |
| `RESULTS.md` | Baseline vs variant scores and verdict |

See `RESULTS.md` for the full report and reproduction steps.
