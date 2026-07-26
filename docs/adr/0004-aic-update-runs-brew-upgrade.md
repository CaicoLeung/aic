# ADR 0004: `aic update` runs `brew upgrade aic` for Homebrew installs

- **Status:** Accepted
- **Date:** 2026-07-26
- **Supersedes:** [ADR 0001](./0001-self-update-homebrew-guard.md)

## Context

ADR 0001 decided that `aic update` for Homebrew-installed binaries should print
guidance ("aic was installed via Homebrew. Update it with `brew upgrade aic` …")
and exit without modifying anything. This prevented the `self_update` crate from
racing with `brew upgrade`, at the cost of requiring brew users to remember a
different command.

After living with ADR 0001 for three weeks, the guidance-only step is the single
regretted consequence: `aic update` should Just Work regardless of install
channel.

## Decision

`run_update()` now **delegates directly to `brew upgrade aic`** for Homebrew
installs. Instead of printing guidance text and returning `Ok(())`, it spawns
`brew upgrade aic` as a child process with inherited stdio. On success (brew
exit 0), it returns silently — brew's own output is the only output. On non-zero
exit, an anyhow error propagates (with brew's exit code in the message), and
main's error handler prints the error context.

Design principles:

- **Thin pass-through.** No aic-authored output on the success path. Brew's own
  progress and version output speaks for itself.
- **PATH-only lookup.** `Command::new("brew")` — no `${HOMEBREW_PREFIX}/bin/brew`
  fallback. The Cellar-based detection path (the dominant case) guarantees brew
  is on PATH. An OS `NotFound` error propagates honestly.
- **Short formula name.** `brew upgrade aic` — not the fully-qualified tap form
  `CaicoLeung/aic/aic`. Brew resolves the short name correctly for any single
  installed formula.
- **Extracted helper.** `brew_upgrade_command()` returns the `Command` (without
  running it) so tests can assert the invocation contract — program name and
  args — without shelling out.

## Consequences

- **Positive:** `aic update` now works identically ("just update the binary")
  regardless of install channel. Brew users no longer need to know a different
  command.
- **Positive:** The `self_update`/`brew` race concern from ADR 0001 is still
  avoided — we invoke the correct updater (`brew upgrade`) rather than having
  two updaters fight over the same binary.
- **Positive:** The extracted `brew_upgrade_command()` helper makes the
  invocation contract testable in CI.
- **Negative:** `aic update` now spawns a subprocess for brew users. On failure
  (brew non-zero exit or not found), the error path adds an anyhow line after
  brew's own output. Acceptable — this only fires on errors, and the extra
  context is useful.
- **Negative:** `brew` must be on PATH. If the Homebrew detection fires via the
  `HOMEBREW_PREFIX` fallback in a restricted shell where `brew` is absent,
  `aic update` fails with an OS error rather than printing the old guidance.
  This is the documented pathological edge case from ADR 0001; we accept it to
  keep the implementation path simple.

## Alternatives considered

- **Print a lead line before spawning brew.** Rejected — adds noise on the
  success path, counter to the thin-pass-through goal.
- **Fall back to old guidance on brew-not-found.** Rejected — masks the real
  problem (no brew on PATH) and adds a branch for a pathological case.
- **Use the fully-qualified tap name (`CaicoLeung/aic/aic`).** Rejected —
  verbose, and the short name resolves correctly for the normal single-install.
