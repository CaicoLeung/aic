# ADR 0011: Explicit `backend_kind` discriminator (supersedes ADR 0010 §Selection)

- **Status:** Accepted
- **Date:** 2026-08-09

## Context

ADR 0010 added the CLI-agent Backend and selected it **implicitly**: CLI mode is
active exactly when the flat `command` field is non-empty, otherwise the
API-provider Backend is active. Two problems surfaced once that design was used:

1. **The active Backend is invisible.** Nothing in the config *says* which
   Backend is active — the user infers it from "`command` is set." In `aic setup`
   the two backends are sibling menu rows and the active one surfaces only as a
   parenthetical. Users reported they could not tell which mode they were in.
2. **Flat, orphaned fields.** `command`/`args`/`timeout_secs` are three
   unrelated top-level keys; `args`/`timeout_secs` have no visible owner.

The CLI-agent Backend is **unreleased** (on `add-cli-agent-support` at the time
of this ADR; no shipped config contains `command`), so a schema change is
costless now and expensive the moment it ships.

## Decision

Make the active Backend an **explicit, named value** rather than an inference.
Add a `backend_kind` config field taking `"api"` or `"cli"`; it is the single
authoritative selector.

```toml
backend_kind = "cli"          # or "api"; absent ⇒ "api"

# API-provider Backend (backend_kind = "api"):
backend   = "openai"          # the Provider name (unchanged, released field)
api_key   = "sk-…"
model     = "…"
base_url  = "…"               # optional

# CLI-agent Backend (backend_kind = "cli"):
command      = "claude"
args         = ["-p", "{prompt}"]
timeout_secs = 60
```

### Semantics — authoritative discriminator, dormant fields allowed

The discriminator is authoritative; no field-population is ever silently
inferred to *override* it. The inactive Backend's fields may coexist in the
file as **dormant** config — preserved across backend switches and ignored at
run time — so switching the active Backend never wipes what was entered for
the other:

| Config | Result |
| --- | --- |
| `backend_kind` absent | API-provider Backend (the historical default) |
| `backend_kind = "api"` | API-provider Backend |
| `backend_kind = "cli"` | CLI-agent Backend |
| `backend_kind = "cli"` but no `command` | **error** — "backend_kind = cli but no command set" |
| `backend_kind = "api"` but `command` set | API-provider Backend; `command` is **dormant** (preserved for a later switch back) |
| `backend_kind = "cli"` but `api_key` set | CLI-agent Backend; `api_key` is **dormant** (preserved for a later switch back) |
| `backend_kind` absent but `command` set | **error** — ambiguous; the wizard always writes `backend_kind` when a command is present, so this only arises from a manual edit |

The last row is the crux: the lenient "infer CLI from `command`" rule is
deliberately rejected. Allowing it would let the file *lie* (`backend_kind =
"api"` silently ignored because a command is present) — recreating the exact
invisible-mode confusion this ADR exists to fix. An *explicit* `backend_kind`
alongside the other Backend's fields does not lie: it states the active mode
and keeps the rest dormant, which is exactly what makes non-destructive
switching possible.

### Why a discriminator, not a grouped table or a flat rename

- **Flat rename `command` → `cli_agent`** (the original proposal): renames the
  selection lever but mode stays inferred and `args`/`timeout_secs` stay
  orphaned. Does not make the Backend visible.
- **Grouped `[cli_agent]` table** (presence = CLI mode): self-documenting and
  isomorphic to `CliSpec`, but mode is still *inferred from structure*, not
  stated; an empty table or a stray `command` outside it reopens the inference
  question.
- **Explicit discriminator (chosen):** the mode is a literal value the user and
  the code read directly. The cost is a consistency-validation pass
  (discriminator ↔ populated fields); that cost is the price of the visibility
  this ADR buys, paid once in `Config::validate`.

`command`/`args`/`timeout_secs` stay **flat** (not nested) — the discriminator
carries the grouping semantics; nesting would duplicate it.

### Relation to ADR 0010

Supersedes **only** ADR 0010's "Selection" section. Everything else in 0010
stands: the generic command-template shape, headless/print-only invocation,
least-permission presets, the prompt-injection boundary, the `Backend` enum
dispatch, and reliability/retry behavior. `Config::active_cli_command()` is
replaced by a `backend_kind`-gated read of `command`.

## Consequences

- The active Backend is now a named field — readable in the config file and
  showable directly in `aic setup` (mode-first on first run) and `aic list`.
- `Config::resolve_backend()` reads `backend_kind` as authoritative. It errors
  only on a CLI selected but unconfigured (`backend_kind = "cli"` with no
  `command`) or on an ambiguous `command` with no discriminator; the inactive
  Backend's fields are allowed as dormant config (preserved across switches),
  not rejected as conflicts.
- `backend_kind` absent ⇒ API-provider Backend, so released configs are
  unchanged — **no migration for released configs**. Unreleased `command`
  configs must add `backend_kind = "cli"`.
- `aic-web` (which parses the `Provider` enum, not `backend_kind`) is unaffected.
