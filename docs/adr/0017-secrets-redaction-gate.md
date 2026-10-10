# ADR 0017: Secrets Redaction Gate — default refuse, one-off opt-out

- **Status:** Accepted
- **Date:** 2026-10-09
- **Issue:** [#155](https://github.com/CaicoLeung/aic/issues/155)

## Context

Every Run sends diff content to a Backend: the batch-plan Diff JSON envelope,
each Batch's Drafted Message diff input (sliced from the same per-file diff
snapshot), and — on the staged path — the single-commit staged diff. A working
tree mid-change routinely contains secret-shaped content: rotated cloud keys,
a developer's `id_ed25519`, a `.env` flip. Committing those is a user choice;
**sending them to a third-party LLM is not a choice the user ever made**, and
it happens silently today.

The gate must also fail *open* on benign content: base64 blobs, long hashes,
and ordinary prose pass through diffs constantly, so a heuristic that fires on
them would train users to reach for the bypass by reflex.

## Decision

A **Redaction Gate** refuses the Run before any Backend call when an initial
payload contains secret-shaped content. The error names every offending file
and pattern kind with masked previews (first 4 chars + `…` — never the full
match) and the `--no-redact` override, then exits non-zero.

1. **Detection is structural, not statistical.** A hardcoded pattern table in
   `src/llm/redact/`: provider token prefixes (`AKIA`/`ASIA`, `ghp_`…,
   `github_pat_`, `sk-`-family, `AIza`, `xox*-`, `sk_live_`/`rk_live_`,
   `glpat-`) with minimum tail lengths, plus PEM `-----BEGIN … PRIVATE
   KEY-----` headers. No entropy scoring and no `api_key =` assignment
   heuristics — both fail on the benign-content contract above. Adding a
   pattern is a one-row table change plus a unit test; nothing is configurable.
2. **The whole diff is scanned** — context and removed lines included. A
   secret being *deleted* is still a secret being sent.
3. **Initial payloads only.** The unstaged path gates the per-file diff
   snapshot once (it feeds every payload of that path); the staged path gates
   its diff before the single messenger call. The confirmation menu's
   Re-generate redraft re-sends content the gate already cleared and stays
   ungated — a pre-commit hook injecting a *new* secret mid-Run is out of
   scope for v1.
4. **The escape hatch is CLI-only.** `--no-redact` mirrors `--hint`: one-off
   Run intent, rejected alongside subcommands, no config field (ADR 0008's
   single-source spirit — a persisted bypass would silently disable a safety
   gate for every future Run). Passing it skips scanning entirely.
5. **Scope is the commit Run.** The resolve workflow sends whole-file content
   and gets the same gate in a tracked follow-up, reusing this detector.

## Consequences

- **Positive:** No secret-shaped diff content reaches a Backend without an
  explicit, per-Run user decision. The refusal names files and kinds, so the
  two-minute fix (unstage, scrub, or knowingly bypass) is on screen.
- **Negative:** False positives block a Run until `--no-redact`. Structural
  prefixes keep this rare (issue #155's benign-content tests pin it), but a
  36+ character run after `ghp_` in generated test data would trip it — the
  override exists for exactly that, and the pattern table grows as real
  families are reported.
- **Neutral:** AWS secret access keys (unstructured 40-char strings) are
  deliberately *not* detected alone — only their `AKIA`/`ASIA` key-ID
  counterparts. An AWS pair in a diff usually ships both.
