# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

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

### Refactoring

- Rename system_prompt to git_message and update prompts
- Replace env var parsing with centralized config

### Testing

- Rewrite tests to use temporary repositories

### Miscellaneous

- Add .gitignore to exclude build artifacts
- Add .DS_Store to .gitignore
- Add Rust toolchain configuration


