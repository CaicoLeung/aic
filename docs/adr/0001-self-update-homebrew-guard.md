# ADR 0001: Guard `aic update` against Homebrew installs

- **Status:** Accepted
- **Date:** 2026-07-04

## Context

`aic` is distributed through two channels that each bring their own updater:

- **In-place self-update** via the `self_update` crate, invoked by the `aic update` subcommand (`src/update.rs`). It downloads the latest GitHub release archive and overwrites `current_exe()` on disk.
- **Homebrew**, via the custom tap `CaicoLeung/homebrew-aic` auto-published by cargo-dist. Brew installs the binary into a versioned Cellar directory (e.g. `/opt/homebrew/Cellar/aic/<ver>/bin/aic`) and symlinks it into the bin dir.

When both channels target the same binary, they fight. `self_update` resolves the symlink and replaces the file *inside* the Cellar, behind Homebrew's back. After that:

- `brew upgrade aic` either reverts the self-update or reports a version that no longer matches the bytes on disk.
- `brew list --versions` and `brew doctor` report stale state.
- The user has two updaters racing on the same artifact, with no clear source of truth.

We needed to decide how `aic update` should behave for users who installed via Homebrew.

## Decision

`run_update()` detects Homebrew installs before invoking the self-updater and short-circuits with guidance instead. Detection checks whether `current_exe()` contains a `Cellar` path component, or resolves under `HOMEBREW_PREFIX`. When detected, `aic update` prints a message directing the user to `brew upgrade aic` and exits successfully without modifying anything.

Shell- and PowerShell-installed users keep the in-place updater; Homebrew users get `brew upgrade`. The two updaters never touch the same binary.

## Consequences

- **Positive:** Homebrew installs stay consistent with brew's recorded version. No silent corruption from a race between `self_update` and `brew upgrade`.
- **Positive:** The `aic update` subcommand remains useful for the majority of users (shell/PowerShell installs); only brew users are redirected.
- **Negative:** Brew users have to know to use a different command. This is mitigated by the printed guidance — `aic update` tells them exactly what to run rather than failing opaquely.
- **Negative:** Detection relies on path heuristics (Cellar component / `HOMEBREW_PREFIX`). A user who symlinks a non-brew install into a Cellar path would be misidentified, but this is pathological and not a realistic install shape.

## Alternatives considered

- **Remove the `update` subcommand entirely and rely on package managers.** Rejected: shell/PowerShell-installed users (the default `dist` installers) would lose convenient updates and have to re-run the installer manually for every release.
- **Let `self_update` run against brew installs and document the caveat.** Rejected: silent version desync is the worst outcome — the user sees "Updated to version X" while brew still believes the previous version is installed. The guard makes the conflict loud and redirects to the correct updater.
