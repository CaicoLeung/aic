# aic

> **简体中文:** [README.zh-CN.md](./README.zh-CN.md)

AI-powered git commit tool that writes Conventional Commit messages for you — and splits your work into **hunk-level** atomic commits, not file-level ones.

🌐 **Website:** <https://caicoleung.github.io/aic-web/>

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml)
[![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

---

## ✨ The headline: hunk-level commits, not file-level

Most AI commit tools treat a **file** as the atomic unit. aic treats a **hunk** (a single contiguous code change) as the atomic unit.

Edit one file in three unrelated ways and `aic` produces three clean, atomic commits — no manual `git add -p`, no mixed-concern commits.

```
You edited ONE file in three unrelated ways:

  src/auth.rs
    ├─ hunk 1  fix token-expiry check     →  fix(auth): correct token expiry check
    ├─ hunk 2  add OAuth2 login provider  →  feat(auth): add OAuth2 login provider
    └─ hunk 3  reformat imports           →  style(auth): tidy imports

Other tools:  1 commit  "update src/auth.rs"     ❌ mixed concerns, muddy history
aic:          3 commits, one per logical change  ✅
```

**Why it's safe:**

- **Exact-partition validation** — every hunk is assigned to exactly one commit, with no overlaps and no gaps, so nothing is lost or double-committed. The plan is rejected if a single hunk is missing or out of range.
- **Context-aware staging** — selected hunks are rebuilt into a patch and staged with `git apply --cached`, which relocates each hunk by its surrounding context lines so it still lands correctly after an earlier commit shifted line numbers.
- **Live reasoning** — aic streams the model's thinking as it decides the split, so you can see *why* each hunk was grouped before the commits land.

## Features

- **Hunk-level batch splitting** — one file, many concerns? aic splits per-hunk into atomic commits (`git add -p` style, fully non-interactive)
- **Multi-provider** — OpenAI, Anthropic, Gemini, DeepSeek, Groq, xAI, Mistral, OpenRouter, Perplexity, Together, Ollama, and any OpenAI-compatible server
- **Conflict resolution** — mid-merge? `aic resolve` proposes per-file resolutions you review and approve, then finalizes the merge
- **Interactive setup** — `aic setup` walks you through provider, API key, and model selection
- **Conventional Commits** — messages follow the [Conventional Commits v1.0.0](https://www.conventionalcommits.org/) spec
- **Configurable** — config file, environment variables, or per-run override

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

Update with `brew upgrade aic`. Homebrew installs are detected automatically, so `aic update` redirects you to brew without modifying anything.

### Build from source

```sh
git clone https://github.com/CaicoLeung/aic.git
cd aic
cargo build --release
# binary at target/release/aic
```

### Shell completion (Tab)

Install completions with one command — `aic` prompts you to pick a shell (defaulting to your `$SHELL`) and writes the script to its conventional location:

```sh
aic completion            # pick a shell interactively, then install
```

Reload your shell (`exec $SHELL`) and Tab completion is active. Supported: `bash`, `fish`, `zsh`, `nushell`. `bash` and `fish` are autoloaded; `zsh` is autoloaded under Homebrew (its `site-functions` dir is already on `$fpath`) but otherwise needs a `$fpath` entry; `nushell` needs a `source` line in `config.nu` (printed after install).

## Quick Start

```sh
# 1. Configure your LLM provider
aic setup

# 2. Stage some files and commit
git add src/main.rs
aic
# → feat: add CLI argument parsing
#   Created commit abc1234

# 3. Or run with NOTHING staged — aic splits your working-tree
#    changes into hunk-level atomic commits automatically
aic
```

## Usage

| Command       | Description                                                                                                            |
| ------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `aic`         | Commit staged files with one message. If nothing is staged, batch-plan all unstaged changes into **hunk-level** atomic commits. |
| `aic resolve` | Resolve git merge conflicts via the LLM. Proposes per-file resolutions to review, then finalizes the merge.            |
| `aic setup`   | Interactive wizard to pick provider, enter API key, and select model.                                                  |
| `aic list`    | Show resolved config: provider, model, and where each value comes from (env / config / default).                       |
| `aic update`  | Update aic to the latest version from GitHub Releases.                                                                 |

## How It Works

```
aic
  ├─ staged files? → diff staged files → LLM writes the message → commit
  └─ nothing staged? → diff workdir → LLM partitions EVERY hunk into batches:
        for each batch (reasoning streamed live as the model thinks):
          stage its hunks via `git apply --cached` (context-relocated)
          → LLM writes the message → commit

aic resolve
  └─ conflicted repo? → for each conflicted file:
        LLM proposes a resolution → validate markers (retry once)
        → review diff → apply? [y/n] → git add
        → finalize (git --continue) when all resolved
```

All commit messages follow Conventional Commits (`feat:`, `fix:`, `refactor:`, etc.) with an optional body.

### Inside the hunk splitter

1. **Diff once** — aic captures each file's workdir-vs-HEAD diff and numbers its hunks (1, 2, 3, …).
2. **Partition** — the model assigns every hunk index to exactly one batch, grouped by logical concern. Each batch may carry hunks from several files; one file's hunks may spread across several batches.
3. **Validate** — the plan is checked to be an *exact partition*: every hunk covered once, no overlaps, no out-of-range or unknown-file references. If validation fails, no commit is made.
4. **Stage & commit** — for each batch, the chosen hunks are rebuilt into a patch and applied with `git apply --cached`, which relocates hunks by context so they land correctly even after an earlier commit shifted line numbers. Then the message is generated and the commit is created.

## Configuration

Config file: `~/.config/aic/config.toml`

### Environment variables

| Variable            | Purpose                                        | Default          |
| ------------------- | ---------------------------------------------- | ---------------- |
| `LLM_BACKEND`       | Provider name                                  | `openai`         |
| `LLM_API_KEY`       | API key (falls back to provider-specific vars) | —                |
| `LLM_MODEL`         | Model ID override                              | Provider default |
| `LLM_BASE_URL`      | Endpoint base URL (Ollama / OpenAI-compatible) | Provider default |
| `AIC_SYSTEM_PROMPT` | Override the commit message system prompt      | Built-in prompt  |

Provider-specific API key env vars (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc.) are also recognized.

### Resolution order

For each of `backend`, `api_key`, `model`, and `base_url`:

1. Generic env var (`LLM_BACKEND`, `LLM_API_KEY`, `LLM_MODEL`)
2. Provider-specific env var (API key only)
3. Config file (`~/.config/aic/config.toml`)
4. Built-in default

### Supported providers

| Provider          | Default model                              | Env key                                                       |
| ----------------- | ------------------------------------------ | ------------------------------------------------------------- |
| OpenAI            | `gpt-5-mini`                               | `OPENAI_API_KEY`                                              |
| Anthropic         | `claude-haiku-4-5`                         | `ANTHROPIC_API_KEY`                                           |
| Gemini            | `gemini-2.5-flash`                         | `GEMINI_API_KEY`                                              |
| DeepSeek          | `deepseek-v4-flash`                        | `DEEPSEEK_API_KEY`                                            |
| Groq              | `llama-3.3-70b-versatile`                  | `GROQ_API_KEY`                                                |
| xAI               | `grok-4.3`                                 | `XAI_API_KEY`                                                 |
| Mistral           | `mistral-small-latest`                     | `MISTRAL_API_KEY`                                             |
| OpenRouter        | _(model required)_                         | `OPENROUTER_API_KEY`                                          |
| Perplexity        | `sonar`                                    | `PERPLEXITY_API_KEY`                                          |
| Together          | `meta-llama/Llama-3.3-70B-Instruct-Turbo`  | `TOGETHER_API_KEY`                                            |
| Ollama            | `llama3.3`                                 | _(no key; override URL via `LLM_BASE_URL`)_                   |
| OpenAI-compatible | _(model required)_                         | _(optional; set `LLM_BASE_URL` + `LLM_MODEL`)_                |

OpenRouter and the OpenAI-compatible provider have no default model — set `LLM_MODEL` explicitly. The OpenAI-compatible provider also requires `LLM_BASE_URL` and routes through the OpenAI client against any server that speaks the OpenAI chat-completions API (LM Studio, vLLM, gateways).

## Resolving merge conflicts

Run `aic resolve` when your repo is mid-merge. It reads each conflicted file, proposes a marker-free resolution, shows you the diff, and asks `apply?` per file. Approve the ones you trust; the rest stay untouched. When nothing is left unmerged, it runs the merge's `--continue` for you.

You can also run plain `aic` in a conflicted repo — it notices and offers to hand off to resolve, and a commit guard blocks any commit that still carries conflict markers.

**v1 limits:** `aic resolve` handles conflicted **merge** state — a rebase or `am` in flight is detected and refused. Binary, oversized, and delete/modify conflicts are skipped with a reason for you to resolve by hand. Finalize is all-or-nothing: `--continue` blocks on any unmerged path, and the hand-off tells you exactly what's left.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) — branch rules, commit style, and what to expect from review. (简体中文: [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md))

## License

[MIT](https://github.com/CaicoLeung/aic/blob/main/LICENSE)
