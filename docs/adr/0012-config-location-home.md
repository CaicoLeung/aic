# ADR 0012: Config location hardcoded to `~/.config/aic`

- **Status:** Accepted
- **Date:** 2026-08-09

## Context

Since the first release, `config_path()` has been
`dirs::config_dir().join("aic").join("config.toml")`. `dirs::config_dir()` is
OS-native: `~/.config` on Linux, `~/Library/Application Support` on macOS,
`%APPDATA%` on Windows. So on macOS the config file actually lived at
`~/Library/Application Support/aic/config.toml`.

Meanwhile **every piece of documentation already claimed `~/.config/aic/config.toml`**
—the module doc-comment on `config.rs`, and both the English and zh-CN READMEs:

> The config file is the single source of truth: `~/.config/aic/config.toml`.

The docs and the code had drifted apart. The macOS path was also an outlier:
`aic` is a developer tool whose users uniformly think in `~/.config` terms, and
the cargo-dist installer already writes its receipt to `~/.config/aic/`, so the
directory existed on most machines anyway.

## Decision

Resolve the drift in favor of the docs. Replace `dirs::config_dir()` with
`dirs::home_dir().join(".config").join("aic").join("config.toml")`, so the
config file location is `~/.config/aic/config.toml` **on every platform**.

```rust
pub fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join(".config").join("aic").join("config.toml"))
}
```

`XDG_CONFIG_HOME` is intentionally **not** honored: the whole point is one
fixed path that matches the docs. Users who set `XDG_CONFIG_HOME` accept that
their redirection is ignored.

### One-time location migration

Existing macOS users have a real config at the old path. A one-time
**location migration** (`Config::migrate_location`), runs once per startup
before `migrate_if_stale`, with these exact semantics:

| State | Action |
| --- | --- |
| old path has config, new path does not | **copy** old → new, then **delete** old |
| new path already has a config | **skip silently** — new wins, old file left in place |
| old and new resolve to the same path (plain Linux) | no-op |
| old path absent | no-op |

It is idempotent — after the first successful run, the new path exists, so
every later run takes the "skip silently" row. A one-line notice is printed to
stderr **only when a file is actually moved**, matching the "transparency is
non-negotiable" ethos of `migrate_if_stale`. A failure is logged and never
blocks the run.

### Why not the alternatives

- **Keep `dirs::config_dir()` (status quo).** The docs stay wrong on macOS;
  users hunt for their config under `~/.config` and don't find it.
- **Override macOS only (`#[cfg(target_os = "macos")]`).** Smaller blast
  radius, but adds a platform branch to test and still leaves the docs/code
  divergence live on the code path for Windows. One path everywhere is simpler.
- **Pure XDG (honor `XDG_CONFIG_HOME`).** Most "correct" on Unix, but
  reintroduces a hidden variable that can move the file out from under the docs
  and the user's mental model. aic's audience doesn't need it.

## Consequences

- The config file is at `~/.config/aic/config.toml` on all platforms; the
  README and module docs are now truthful.
- macOS users with an existing config are migrated automatically on their next
  run; their old file at `~/Library/Application Support/aic/config.toml` is
  deleted after a successful copy. The now-empty `~/Library/Application
  Support/aic/` directory is left in place (harmless; out of scope to remove).
- `XDG_CONFIG_HOME` is no longer consulted; users who relied on it must move
  their config to `~/.config/aic/` manually (location migration does this
  automatically on the next run, since the old `dirs::config_dir()`-resolved
  path is treated as the legacy source).
- `config_path()` still returns `Option<PathBuf>`; the only `None` source is
  `dirs::home_dir()` failing, which is unchanged in spirit from before.
- Builds on ADR 0008 (config is the single source of truth) — there is now
  literally one path, on one location, across every platform.
