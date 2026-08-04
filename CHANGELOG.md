# Changelog

All notable changes to this project will be documented in this file.

## [0.4.6] - 2026-08-04

> 🎉 **0.4.6** — 1 change · 1 contributor

### Bug Fixes

- Erase reasoning block without leaving a blank gap (#86)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.4.5] - 2026-08-04

> 🎉 **0.4.5** — 4 changes · 1 contributor

### Features

- Interactive installer, drop generate-completion (#76)

### Bug Fixes

- Hide cursor for the whole reasoning stream (#77)
- Skip sleep for zero backoff
- Return underlying error as source


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.4.4] - 2026-08-02

> 🎉 **0.4.4** — 2 changes · 1 contributor

### Features

- ESC back-navigation + curated model picker (#74)

### Bug Fixes

- Cap reasoning to a rolling 12-row window, keep it visible (#73)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.4.3] - 2026-08-02

> 🎉 **0.4.3** — 2 changes · 1 contributor

### Features

- Arrow-key driven interactive configuration
- Stream reasoning as a sliding window, cut flicker (#68)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.4.2] - 2026-08-02

> 🎉 **0.4.2** — 3 changes · 1 contributor

### Features

- Default to yes on empty input

### Bug Fixes

- Eliminate spinner flicker during reasoning stream
- Drain trailing reasoning, cap scroll, harden prints


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.4.1] - 2026-08-01

> 🎉 **0.4.1** — 6 changes · 1 contributor

### Features

- Resolve contributor @handles via GitHub API (#54)
- Contributor avatars in release notes (#55)
- Contributor table — avatar + @handle below (#56)
- Cleaner release notes (noise, TL;DR, breaking, contributors) (#57)

### Bug Fixes

- Retry empty model responses instead of aborting the batch run (#59)
- Survive pre-commit hooks that restage whole files (#61)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.4.0] - 2026-08-01

> 🎉 **0.4.0** — 7 changes · 2 contributors

### Features

- Add `generate-completion` command to generate shell completions (#16)
- Colorize commit type prefixes in output (#39)
- Inset run output and wrap commit body to terminal width (#51)

### Bug Fixes

- Recurse untracked directories in workdir diff
- Include error chain in abort message
- Make git test suite cross-platform for windows CI

### Documentation

- Add bilingual CONTRIBUTING guide (#36)


### Contributors
🎉 Thanks to the 2 contributors below!
<table><tr><td align="center"><a href="https://github.com/Paul-16098"><img src="https://github.com/Paul-16098.png?size=96" width="64" height="64"><br><sub><b>@Paul-16098</b></sub></a></td><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.3.7] - 2026-07-31

> 🎉 **0.3.7** — 3 changes · 1 contributor

### Bug Fixes

- Commit authored messages via the git CLI (#19) (#22)

### Documentation

- Document hunk-level atomic commit splitting
- Add Simplified Chinese translation (#14)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.3.6] - 2026-07-30

> 🎉 **0.3.6** — 1 change · 1 contributor

### Features

- Streaming multi-batch commit splitting with testable orchestration (#13)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.3.5] - 2026-07-30

> 🎉 **0.3.5** — 2 changes · 1 contributor

### Features

- Per-hunk batch splitting with a live reasoning view (#12)

### Documentation

- Document aic resolve for v0.3.0 (#11)


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.3.0] - 2026-07-29

> 🎉 **0.3.0** — 4 changes · 1 contributor

### Features

- Add AI merge-conflict resolution and commit guard
- Enhance user feedback and fix marker detection in staged blobs
- Add e2e test infrastructure for resolve workflow
- Expand e2e coverage to cherry-pick/revert finalize + conflict-kind skips


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.2.3] - 2026-07-26

> 🎉 **0.2.3** — 1 change · 1 contributor

### Bug Fixes

- Make gh release create idempotent on re-runs


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.2.2] - 2026-07-26

> 🎉 **0.2.2** — 5 changes · 1 contributor

### Features

- Add status field to unstaged file diffs and handle deletions in prompt
- Delegate homebrew update to brew upgrade aic
- Add panel-based terminal output with non-TTY support (#4)

### Bug Fixes

- Handle working-tree deletions in add method
- Reject untracked absent paths in add


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.2.1] - 2026-07-25

> 🎉 **0.2.1** — 1 change · 1 contributor

### Features

- Update default model to deepseek-v4-flash


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.2.0] - 2026-07-09

> 🎉 **0.2.0** — 1 change · 1 contributor

### Features

- Expand to 12 providers, refresh default models, add base URL


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.1.7] - 2026-07-08

> 🎉 **0.1.7** — 3 changes · 1 contributor

### Documentation

- Add Homebrew installation instructions
- Add website link
- Add CONTEXT.md with project terminology and concepts


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.1.6] - 2026-07-04

> 🎉 **0.1.6** — 14 changes · 1 contributor

### Features

- Add Homebrew installer and self-update guard
- Add workflow to update changelog on release
- Verify releases with embedded zipsign public key
- Add cargo-deny supply chain checks and weekly advisory scan
- Add preflight checks, smoke tests, and archive signing
- Add weekly token probe workflow
- Add version comparison and Cargo.toml version update functions
- Add prepare-release.sh for release preparation

### Bug Fixes

- Handle path() Result and serialize CWD tests
- Allow dist plan to run with dirty working directory
- Use cargo fmt --all instead of bare rustfmt
- Update binary search path for dist profile builds

### Documentation

- Add ADRs for self-update guard and signed updates, plus agent docs
- Add release procedure documentation


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.1.4] - 2026-05-20

> 🎉 **0.1.4** — 6 changes · 1 contributor

### Features

- Add batch plan validation and improve error handling
- Add automatic rustfmt formatting before commit
- Enable compression for self_update updates
- Add unix-archive setting

### Bug Fixes

- Specify tag for git-cliff changelog generation

### Documentation

- Add v0.1.1 entry with feature and refactor highlights


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.1.2] - 2026-05-20

> 🎉 **0.1.2** — 2 changes · 1 contributor

### Features

- Add self-update command for aic
- Add update command for self-update via GitHub Releases


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.1.1] - 2026-05-18

> 🎉 **0.1.1** — 7 changes · 1 contributor

### Features

- Add style commit parser rule
- Add colored output for commit messages
- Add animated startup banner
- Add scoped diff parsing for function-level grouping

### Documentation

- Add initial README
- Add -L flag to curl command in install instructions
- Add changelog badge and contributing guidelines


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>
## [0.1.0] - 2026-05-17

> 🎉 **0.1.0** — 15 changes · 1 contributor

### Features

- Initialize Rust project with rig-core, tokio, and dependencies
- Add prompt configuration module with system and batch plan prompts
- Add multi-provider LLM client with streaming support
- Add clap, git2, schemars, and serde derive
- Add LLMAgent with typed prompt support
- Add git operations module
- Add Generator module for commit message and batch planning
- Add main entry point with staged/unstaged commit workflow
- Add CLI and config management for setup
- Add CLI subcommands for setup and list using clap
- Add progress spinner for commit generation
- Enhance commit output with emojis and body details
- Add CI and release automation
- Add automatic changelog generation with git-cliff
- Allow dirty CI workflows for git-cliff


### Contributors
🎉 Thanks to the 1 contributor below!
<table><tr><td align="center"><a href="https://github.com/CaicoLeung"><img src="https://github.com/CaicoLeung.png?size=96" width="64" height="64"><br><sub><b>@CaicoLeung</b></sub></a></td></tr></table>

