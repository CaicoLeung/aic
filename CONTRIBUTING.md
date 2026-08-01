# Contributing to aic

Thanks for wanting to contribute! 🎉 This guide keeps the review loop fast and the history clean.

> 简体中文版见 [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md)

## Before you start

- **Read the ADRs first** — architecture decisions live in [`docs/adr/`](./docs/adr/). If your change touches the architecture, check whether a relevant ADR exists and add one if your change is a decision worth recording.
- **Open an issue or discuss first** for non-trivial features. A quick sanity check upstream saves everyone a round-trip.

## Local setup

```bash
cargo build          # stable toolchain, pinned via rust-toolchain.toml
cargo test           # unit tests live inline in src/ (#[cfg(test)] modules)
```

## Git history rules

- **No merge commits.** Keep the branch linear — rebase on `main` instead of merging it in. Merge commits are rejected at merge time.
- **Conventional Commits** for commit messages (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`, ...) — aic writes these itself, so the project stays consistent with its own tool.

## Before submitting a PR

1. Run `cargo fmt --all` and commit the formatting
2. Run `cargo clippy --all-targets -- -D warnings` and fix all warnings — CI fails on any warning
3. Run `cargo test` — add tests for new behaviour (inline `#[cfg(test)]` modules in `src/`, or integration tests under `tests/`)
4. Fill in the [PR template](./.github/pull_request_template.md) — what, why, and how it was verified

## What to expect from review

- CI enforces `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and `cargo deny` (advisory/license checks) on every PR
- A maintainer will review; you may be asked for changes — that's normal, keep the diff focused on what was requested
- Changes are merged with **squash** once CI is green and the review is approved
- Branch rules reject merge commits, so rebase-based updates are expected — if your branch was rewritten, it was done to keep history linear, not to erase your work

## Releasing

Releases are handled by the maintainers via `scripts/release.sh` (`prepare-release.sh` for the changelog). You don't need to bump versions in your PR.
