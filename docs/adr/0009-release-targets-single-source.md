# ADR 0009: Release targets have a single source of truth

- **Status:** Accepted
- **Date:** 2026-08-09

## Context

`cargo-dist` accepts the build-target list (`targets`) in two places: the
`[dist]` table of `dist-workspace.toml`, and the `[package.metadata.dist]` table
of `Cargo.toml`. When both are present, dist **merges** them (it neither errors
nor picks a winner), so the targets are honoured either way.

That tolerance is a foot-gun. PR #113 added three Linux targets
(`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`arm-unknown-linux-gnueabihf`) by appending a second `targets` list to
`Cargo.toml` while `dist-workspace.toml` already declared its own — different —
five-target list. The result was two disagreeing declarations of the same
concept, neither obviously authoritative: the same "silent override" shape
[ADR 0008](0008-config-single-source-of-truth.md) removed from provider config.

Separately, the release smoke test skipped cross-compiled targets by **name**
(a `*aarch64-unknown-linux-gnu*` glob), on the assumption they were built on an
x86-64 runner. dist 0.31 now builds `aarch64-unknown-linux-{gnu,musl}` on native
`ubuntu-22.04-arm` runners, so that glob was already stale — it skipped targets
that *can* run natively (lost coverage) while still failing to cover
`arm-unknown-linux-gnueabihf` (genuinely cross-compiled, no native armhf runner).

## Decision

`dist-workspace.toml` is the **only** home for the release target matrix. The
`[package.metadata.dist]` block is not added to `Cargo.toml`; if it ever
reappears it must not redeclare `targets`. Adding or removing a release target is
a one-file change.

The release smoke test is likewise decoupled from target **names**: it attempts
to execute the built binary and skips only when the kernel rejects a foreign
architecture (`ENOEXEC` → "Exec format error" / "cannot execute binary file").
A native binary that compiles but panics on startup still fails the job; a
cross-compiled binary that can't run here is skipped with a clear message, and
remains covered by dist's `--print=linkage` check.

## Consequences

- **Positive:** One source of truth for the target matrix; no silent divergence
  between two files. Adding a target can't accidentally ship only half the
  matrix.
- **Positive:** The smoke test regains coverage for native ARM64 Linux targets
  and auto-handles any future cross-compiled target without editing a skip list.
- **Negative:** `dist-workspace.toml` is owned by `dist init`; a maintainer who
  re-runs `dist init` should confirm the target list is preserved. dist preserves
  existing values, so the risk is low.
