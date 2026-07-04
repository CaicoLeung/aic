# aic

AI-powered git commit message generator. Analyzes your staged or unstaged changes and writes conventional commit messages for you.

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml)
[![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

## Features

- **Multi-provider** — OpenAI, Anthropic, Gemini, DeepSeek, Groq, and Ollama
- **Batch commits** — no staged files? aic splits your unstaged changes into logical atomic commits
- **Interactive setup** — `aic setup` walks you through provider, API key, and model selection
- **Conventional Commits** — messages follow the [Conventional Commits v1.0.0](https://www.conventionalcommits.org/) spec
- **Configurable** — config file, environment variables, or per-override

## Installation

### Binary (recommended)

**macOS / Linux:**

```sh
curl --proto '=https' --tlsv1.2 -sSfL https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.ps1 | iex
```

### Homebrew

**macOS / Linux:**

```sh
brew tap CaicoLeung/aic
brew install aic
```

Update with `brew upgrade aic`. Homebrew installs are detected automatically, so `aic update` will redirect you to brew without modifying anything.

### Build from source

```sh
git clone https://github.com/CaicoLeung/aic.git
cd aic
cargo build --release
# binary at target/release/aic
```

## Quick Start

```sh
# 1. Configure your LLM provider
aic setup

# 2. Stage some files and commit
git add src/main.rs
aic
# → feat: add CLI argument parsing
#   Created commit abc1234

# Or run with no staging — aic batches unstaged changes automatically
aic
```

## Usage

| Command      | Description                                                                                                            |
| ------------ | ---------------------------------------------------------------------------------------------------------------------- |
| `aic`        | Generate commit messages for staged files. If nothing is staged, batch-plan all unstaged changes into logical commits. |
| `aic setup`  | Interactive wizard to pick provider, enter API key, and select model.                                                  |
| `aic list`   | Show resolved config: provider, model, and where each value comes from (env / config / default).                       |
| `aic update` | Update aic to the latest version from GitHub Releases.                                                                 |

## Configuration

Config file: `~/.config/aic/config.toml`

### Environment variables

| Variable            | Purpose                                        | Default          |
| ------------------- | ---------------------------------------------- | ---------------- |
| `LLM_BACKEND`       | Provider name                                  | `openai`         |
| `LLM_API_KEY`       | API key (falls back to provider-specific vars) | —                |
| `LLM_MODEL`         | Model ID override                              | Provider default |
| `AIC_SYSTEM_PROMPT` | Override the commit message system prompt      | Built-in prompt  |

Provider-specific API key env vars (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc.) are also recognized.

### Resolution order

For each of `backend`, `api_key`, `model`:

1. Generic env var (`LLM_BACKEND`, `LLM_API_KEY`, `LLM_MODEL`)
2. Provider-specific env var (API key only)
3. Config file (`~/.config/aic/config.toml`)
4. Built-in default

### Supported providers

| Provider  | Default model              | Env key                            |
| --------- | -------------------------- | ---------------------------------- |
| OpenAI    | `gpt-4o-mini`              | `OPENAI_API_KEY`                   |
| Anthropic | `claude-sonnet-4-20250514` | `ANTHROPIC_API_KEY`                |
| Gemini    | `gemini-2.0-flash`         | `GEMINI_API_KEY`                   |
| DeepSeek  | `deepseek-chat`            | `DEEPSEEK_API_KEY`                 |
| Groq      | `llama-3.3-70b-versatile`  | `GROQ_API_KEY`                     |
| Ollama    | `llama3.2`                 | _(localhost:11434, no key needed)_ |

## How It Works

```
aic
  ├─ staged files? → diff staged files → LLM generates message → commit
  └─ no staged?    → diff workdir → LLM splits into batches → for each batch:
                        git add → LLM generates message → commit
```

All commit messages follow Conventional Commits (`feat:`, `fix:`, `refactor:`, etc.) with an optional body.

## Contributing

- Run `cargo fmt` before committing
- Run `cargo clippy -- -D warnings` and fix all warnings
- Add tests for new behaviour in `src/` or as integration tests in `tests/`

## License

[MIT](https://github.com/CaicoLeung/aic/blob/main/LICENSE)
