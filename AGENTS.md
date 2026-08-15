# Repository Guidelines

## Project Overview

`aic` is an AI-powered git commit message generator (Rust CLI, edition 2024). It produces hunk-level atomic Conventional Commits: nothing staged → the LLM plans unstaged hunks into logical Batches → one commit per Batch; something staged → one commit. Two LLM backends: 12 API providers via `rig` (openai, anthropic, gemini, deepseek, groq, xai, mistral, openrouter, perplexity, together, ollama, openai-compatible) or external CLI agents (claude/codex/pi/opencode) run headless. Subcommands: `resolve` (merge-conflict resolution), `setup`, `use`, `list`, `update` (zipsign-verified self-update), `completion`.

## Architecture & Data Flow

Thin binary over a five-domain library (`src/lib.rs`; ADR 0015/0016 layout). `src/main.rs` = `#[tokio::main]` → clap parse → two idempotent config migrations → subcommand dispatch; no subcommand = default commit Run.

Default Run data flow (unstaged path):

1. `src/workflow/run/mod.rs::commit_run` — `git.status()` splits staged/unstaged.
2. `src/git/diff_view.rs::diff_workdir` per file → pure parse in `src/git/diff/` (`FilePatch`, numbered hunks) → `src/git/diff_json/` envelope `{"unstaged_files":[{"path","diff"}]}`.
3. `src/llm/generator/mod.rs::split_patch_streaming` streams `BatchPlanOutput` + reasoning deltas (painted by `src/render/reasoning_feed/`).
4. `generator::validate_batch_plan` — exact hunk partition check; invalid → deterministic fallback `src/workflow/grouping/` (no LLM).
5. Batches drafted concurrently (`MAX_CONCURRENT_DRAFTS = 8`, ADR 0014) → `generate_commit_message` (typed `CommitOutput` schema).
6. Per batch: `src/git/staging/` remaps plan-time hunk indices onto the *current* diff (pre-commit hooks can't desync later batches), then `git apply --cached`.
7. Optional confirm menu (`src/workflow/confirm/`: Commit / Re-generate / Edit / Abort) → `Git::commit` → commit line rendered via `src/render/display/`.

Key patterns (explicitly call out when touching code):

- **Dependency injection**: workflow seams are boxed erased closures (`Resolver`, `Prompt`, `BatchPlanner`, `CommitMessenger` in `src/core/types/mod.rs`) bundled into `RunDeps` / `ResolveDeps`. Production wires real `Generator` closures; tests inject scripted stubs.
- **Enum dispatch, not trait objects, for LLM backends**: `Backend { Rig, Cli }` in `src/llm/mod.rs` — keeps generic `schema<T>` / `stream_typed_with_reasoning<T>` monomorphized. Trait seams (`DisplayWrite`, `ReasoningSink`, `CommandRunner`) are for test injection only.
- **Dual git strategy**: libgit2 for reads (status/diff), real `git` CLI for mutations via `run_git` so hooks/gpgsign run (`src/git/mod.rs`).
- **Error handling**: `anyhow` everywhere, context names user-facing ops; no `thiserror`. Marker errors for downcast only: `CommitDeclined`, `LlmError`/`RetryError`. Git's own stderr surfaces via `nonzero_exit`.
- **Async**: single tokio runtime; `async-trait` only on `CommandRunner` (`src/llm/cli_agent/`). Blocking tty probe wrapped in `spawn_blocking`.
- **Rendering**: all colors through `src/render/palette/` roles (WCAG-tested) — never hand-roll `console::Style`. Markdown painter is pure/zero-IO; ADR 0013 bans TUI frameworks.
- **State**: no globals. Config = `~/.config/aic/config.toml` (TOML, dir 0700/file 0600, resolved via `home_dir`, identical on all OSes — ADR 0012). Config file is the single source of truth; env vars deliberately not read (ADR 0008). `backend_kind` ("api"|"cli") is the authoritative discriminator (ADR 0011).

**Domain vocabulary is normative**: `CONTEXT.md` defines Run, Batch, Block, Drafted Message, Batch staging, Conflict, Resolution, Finalize, Backend, Palette, etc., with Avoid-lists — use these exact terms in code, docs, and commits; never the synonyms. Read `docs/agents/domain.md` rules before exploring; ADR conflicts must be flagged, not silently overridden.

## Key Directories

| Path | Purpose |
|---|---|
| `src/core/` | Cross-cutting: clap CLI, config persistence/resolution/migrations, seam type aliases, self-update, shell completion |
| `src/git/` | Repository surface: `Git` adapter (libgit2 reads + CLI mutations), pure diff parsing, diff→JSON envelope, hunk staging remap, conflict domain |
| `src/llm/` | Backends: provider registry + rig clients, CLI-agent shells, NDJSON decoders, tolerant JSON parse, shared retry, prompts, generator |
| `src/render/` | Terminal output: display, progress/spinners, reasoning feed, markdown, palette, layout, cursor (DSR), commit types |
| `src/workflow/` | User flows composing the rest: run, resolve, setup wizard, confirm, grouping, input primitives |
| `src/e2e/` | `#[cfg(test)]` integration suite (real git tempdirs, stubbed LLM/UI) |
| `docs/adr/` | 16 numbered ADRs with supersession chain — read before non-trivial changes |
| `docs/agents/` | Agent ops: issue-tracker protocol (`gh` CLI), triage labels, domain-doc rules |
| `scripts/` | Release automation (prepare-release, release, release_lib + tests) and provider smoke test |
| `demo/` | VHS tapes + sim scripts rendering the README GIFs |

## Development Commands

```sh
cargo build                              # dev build (no feature flags exist)
cargo run                                # run against the repo you're in
cargo test                               # ALL tests: inline units + src/e2e; hermetic, no API keys
cargo test e2e::                         # e2e suite only
cargo test <name_pattern>                # single test
cargo test -- --nocapture                # show println output
cargo fmt --all -- --check               # CI gate
cargo clippy --all-targets -- -D warnings  # CI gate — any warning fails
cargo deny check                         # advisories/licenses/bans (CI gate)
bash scripts/test_release.sh             # bash unit tests for release_lib.sh (POSIX only)
scripts/smoke-test-providers.sh          # optional: real provider calls, gated on provider API key env vars
```

## Code Conventions & Common Patterns

- rustfmt/clippy **defaults only** — no `rustfmt.toml`/`clippy.toml`; don't add one. CI is the single source of truth.
- Module = directory with `mod.rs` (+ `//!` doc header) and child files; single-file leaves allowed (`git/status.rs`). Tests colocated: sibling `tests.rs` wired via `#[cfg(test)] mod tests;`, or trailing `#[cfg(test)] mod tests` block. No top-level `tests/` dir in practice.
- Naming: `run_*` for command handlers; `*_run` = seam-driven testable core, `*_workflow` = production wiring; pure cores split from IO shells (`apply_use`/`run_use`, `list_lines`/`run_list`).
- Visibility minimal: `pub` only for true API; `pub(crate)` for internal seams; private fields with single construction paths (e.g. `LLM` built only via `ResolvedConfig::to_llm`).
- Rustdoc is dense, with ADR/issue references ("ADR 0005", "issue #78") and rationale on pub items. Deliberate shortcuts are marked `// ponytail:` naming the ceiling.
- **Conventional Commits mandatory** (the tool dogfoods itself). Commit type controls changelog visibility: `git-cliff` skips refactor/test/ci/style/chore — only feat/fix/perf/docs/breaking reach `CHANGELOG.md`.
- Linear history: rebase on main, squash merge; merge commits rejected. Never bump versions in PRs (maintainer-only via `scripts/prepare-release.sh` + `scripts/release.sh`, see `RELEASING.md`).
- LF enforced on all platforms (`.gitattributes`); Windows path/CRLF regressions are a real test target.
- User-facing top-level docs ship as mirrored EN + zh-CN pairs (`README.md`/`README.zh-CN.md`).
- `CLAUDE.md` predates the directory split — its module table and env-var claims are stale; trust source + `CONTEXT.md` + ADRs.

## Important Files

- `src/main.rs` — entry, dispatch. `src/lib.rs` — module root.
- `src/core/config/mod.rs` — `Config`, `ResolvedConfig`, `BackendKind`, migrations, `run_use`/`run_list`.
- `src/core/types/mod.rs` — the seam vocabulary everything injects through.
- `src/workflow/run/mod.rs` — default Run spine (`commit_run`, `default_run`, `default_workflow`, `RunDeps`).
- `src/workflow/resolve/mod.rs` — resolve flow (`resolve_run`, `ResolveDeps`).
- `src/llm/mod.rs` — provider `REGISTRY`, `Backend`/`LlmConfig` enums, `with_agent!` macro.
- `src/git/mod.rs` — `Git` adapter: `run_git`, `commit`, `stage_hunks`.
- `src/e2e/common.rs` — stub factories (`resolver_returning`, `prompt_queue`, `menu_queue`, …) + git fixtures.
- `Cargo.toml` — `self_update` feature set is load-bearing (comment at lines 37–43): dropping `compression-tar-gz` breaks `aic update` on Unix.
- `CONTEXT.md` (normative glossary), `CLAUDE.md` (partially stale), `CONTRIBUTING.md`, `RELEASING.md`.
- `deny.toml` (permissive-license allow-list, yanked=deny), `cliff.toml`, `dist-workspace.toml` (cargo-dist), `keys/zipsign.pub` (embedded via `include_bytes!` in `src/core/update/mod.rs`).

## Runtime/Tooling Preferences

- Rust **stable** channel (`rust-toolchain.toml`, not minor-pinned); edition 2024; MSRV 1.88 (`rust-version`).
- Cargo only: single package, no workspace, **no feature flags**, no justfile/Makefile.
- Release builds use `[profile.dist]` (thin LTO, strip); output at `target/dist/aic`. Releases run through cargo-dist + GitHub Actions — never manually.
- Windows is a first-class CI test target; keep code and tests cross-platform (guard Unix-only bits with `#[cfg(unix)]`).

## Testing & QA

- ~330 tests, two layers, all inside `cargo test`:
  1. Inline `#[cfg(test)]` unit tests per module (pure logic: parsing, decode, config, grouping).
  2. `src/e2e/` — drives `default_run`/`commit_run`/`resolve_run` through the seam bundles against **real git repos in per-test tempdirs** with LLM/UI stubbed as scripted queues. Parallel-safe: each test builds its own `Git::at(tempdir)`, no chdir, no global lock. Zero network, zero API keys.
- Plain libtest — no test framework. `#[tokio::test]` for async; `#[tokio::test(start_paused = true)]` only in `src/render/reasoning_feed/`. No snapshot tests; expectations are inline asserts. No `#[ignore]` tests.
- Patterns: `tempfile::tempdir()` for every git/output path; `temp_env::with_var` for env vars; `parking_lot::Mutex` call counters; `FakeRunner` queued CLI outputs (`src/llm/cli_agent/tests.rs`); real-PTY harness for DSR cursor tests (`src/render/cursor/mod.rs`). Stub queues panic when exhausted so under-scripted tests fail loudly.
- Style: every test fn carries a doc comment naming the contract/regression it pins; names are behavioral sentences (e.g. `commit_batch_loop_aborts_after_partial_commit`). Add tests for new behavior in the same style.
- No coverage tooling. CI (`.github/workflows/ci.yml`) gates: fmt, clippy `-D warnings`, `cargo test` (ubuntu + windows), `cargo deny`; weekly advisory cron.
- Release scripts have their own bash unit suite (`scripts/test_release.sh`, ubuntu CI only).
