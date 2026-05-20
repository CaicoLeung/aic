# Changelog

All notable changes to this project will be documented in this file.

## [0.1.2] - 2026-05-20

### Features

- Add self-update command for aic
- Add update command for self-update via GitHub Releases

### Continuous Integration

- Automate changelog update on release

### Miscellaneous

- Shorten banner text
- Bump version to 0.1.2

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
- Bump version to 0.1.1

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


