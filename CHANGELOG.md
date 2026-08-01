# Changelog

All notable changes to this project will be documented in this file.

## [0.4.0] - 2026-08-01

### Features

- Add `generate-completion` command to generate shell completions (#16)
- Colorize commit type prefixes in output (#39)
- Inset run output and wrap commit body to terminal width (#51)

### Bug Fixes

- Recurse untracked directories in workdir diff
- Include error chain in abort message
- Make git test suite cross-platform for windows CI

### Refactoring

- Split monolithic e2e test into feature modules

### Testing

- Pin staged-files single-commit Run (#26) (#35)
- Harden staged-files single-commit Run assertions (#37)
- Pin cargo fmt before diff capture with hunk alignment (#27) (#38)
- Pin declining finalize when all files resolved by hand (#28) (#44)
- Pin sequence Finalize states (CherryPickSequence/RevertSequence) (#29) (#40)
- Pin multi-file cross-file Batch plans (#31) (#41)
- Pin auto-detect accept-then-reject-all hand-off (#32) (#42)
- Reject empty Batch plan before the loop (#34) (#43)
- Pin am / ApplyMailbox refusal path (#33) (#45)
- Dedupe finalize-state tests via shared helper (#49)
- Pin commit-msg hook veto aborts the Run (#30) (#47)

### Documentation

- Add bilingual CONTRIBUTING guide (#36)

### Continuous Integration

- Matrix cargo test across ubuntu and windows

### Miscellaneous

- Enforce LF via .gitattributes

### Contributors
- @Paul-16098
- @CaicoLeung

## [0.3.7] - 2026-07-31

### Bug Fixes

- Commit authored messages via the git CLI (#19) (#22)

### Refactoring

- Extract shared git-CLI runner (#18) (#21)

### Testing

- Prove git hooks run and can veto during a Run (#20) (#23)

### Documentation

- Document hunk-level atomic commit splitting
- Add Simplified Chinese translation (#14)

### Miscellaneous

- Add pull request template (#17)

### Contributors
- @CaicoLeung

## [0.3.6] - 2026-07-30

### Features

- Streaming multi-batch commit splitting with testable orchestration (#13)

### Contributors
- @CaicoLeung

## [0.3.5] - 2026-07-30

### Features

- Per-hunk batch splitting with a live reasoning view (#12)

### Documentation

- Document aic resolve for v0.3.0 (#11)

### Contributors
- @CaicoLeung

## [0.3.0] - 2026-07-29

### Features

- Add AI merge-conflict resolution and commit guard
- Enhance user feedback and fix marker detection in staged blobs
- Add e2e test infrastructure for resolve workflow
- Expand e2e coverage to cherry-pick/revert finalize + conflict-kind skips

### Refactoring

- Make writer injectable so hand-off wording can be tested (#10)

### Contributors
- @CaicoLeung

## [0.2.3] - 2026-07-26

### Bug Fixes

- Make gh release create idempotent on re-runs

### Refactoring

- Replace panel UI with line-based output (#5)

### Contributors
- @CaicoLeung

## [0.2.2] - 2026-07-26

### Features

- Add status field to unstaged file diffs and handle deletions in prompt
- Delegate homebrew update to brew upgrade aic
- Add panel-based terminal output with non-TTY support (#4)

### Bug Fixes

- Handle working-tree deletions in add method
- Reject untracked absent paths in add

### Contributors
- @CaicoLeung

## [0.2.1] - 2026-07-25

### Features

- Update default model to deepseek-v4-flash

### Contributors
- @CaicoLeung

## [0.2.0] - 2026-07-09

### Features

- Expand to 12 providers, refresh default models, add base URL

### Contributors
- @CaicoLeung

## [0.1.7] - 2026-07-08

### Documentation

- Add Homebrew installation instructions
- Add website link
- Add CONTEXT.md with project terminology and concepts

### Styling

- Apply cargo fmt to unblock 0.1.6 release

### Miscellaneous

- Remove banner display

### Contributors
- @CaicoLeung

## [0.1.6] - 2026-07-04

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

### Refactoring

- Simplify HOMEBREW_PREFIX check with iterator chain

### Documentation

- Add ADRs for self-update guard and signed updates, plus agent docs
- Add release procedure documentation

### Continuous Integration

- Add retry logic for CHANGELOG commit
- Remove announce job
- Add --allow-dirty flag to dist commands

### Miscellaneous

- Skip changelog-update and version-bump commits
- Add docs ignore patterns and homepage
- Update dependencies and bump rust-version
- Skip release commits in changelog
- No staged changes
- Skip empty diff
- No changes to commit

### Contributors
- @CaicoLeung

## [0.1.4] - 2026-05-20

### Features

- Add batch plan validation and improve error handling
- Add automatic rustfmt formatting before commit
- Enable compression for self_update updates
- Add unix-archive setting

### Bug Fixes

- Specify tag for git-cliff changelog generation

### Refactoring

- Update system prompt to emphasize splitting changes

### Documentation

- Add v0.1.1 entry with feature and refactor highlights

### Continuous Integration

- Remove redundant --tag flag from git-cliff
- Add full git fetch depth in release workflow

### Styling

- Condense batch vector literals in tests
- Format method chain across multiple lines

### Contributors
- @CaicoLeung

## [0.1.2] - 2026-05-20

### Features

- Add self-update command for aic
- Add update command for self-update via GitHub Releases

### Continuous Integration

- Automate changelog update on release

### Miscellaneous

- Shorten banner text

### Contributors
- @CaicoLeung

## [0.1.1] - 2026-05-18

### Features

- Add style commit parser rule
- Add colored output for commit messages
- Add animated startup banner
- Add scoped diff parsing for function-level grouping

### Refactoring

- Extract banner function and improve output

### Documentation

- Add initial README
- Add -L flag to curl command in install instructions
- Add changelog badge and contributing guidelines

### Continuous Integration

- Remove duplicate release step from host command

### Styling

- Format clap command attribute

### Miscellaneous

- Add CLAUDE.md to .gitignore
- Update Cargo.toml with dist optimizations and metadata

### Contributors
- @CaicoLeung

## [0.1.0] - 2026-05-17

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

### Refactoring

- Rename system_prompt to git_message and update prompts
- Replace env var parsing with centralized config

### Testing

- Rewrite tests to use temporary repositories

### Miscellaneous

- Add .gitignore to exclude build artifacts
- Add .DS_Store to .gitignore
- Add Rust toolchain configuration

### Contributors
- @CaicoLeung


