# aic

> **简体中文:** [README.zh-CN.md](./README.zh-CN.md)

AI-powered git commit tool that writes Conventional Commit messages for you — and splits your work into **hunk-level** atomic commits, not file-level ones.

🌐 **Website:** <https://caicoleung.github.io/aic-web/>

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml)
[![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

![aic splits one file's mixed edits into three atomic commits](docs/demo/aic-hunk-split.gif)

<sub>One file carrying three unrelated concerns — a `fix`, a `feat`, a `refactor` — becomes three clean atomic commits. Cast: [`docs/demo/aic-hunk-split.cast`](docs/demo/aic-hunk-split.cast) · re-render with `python3 docs/demo/make-cast.py`.</sub>

## Try it in 30 seconds

See the headline on your own machine — **no API key required** (deterministic, offline fixture):

```sh
git clone https://github.com/CaicoLeung/aic.git && cd aic
./scripts/demo.sh
```

You get **three atomic Conventional Commits** (`fix`, `feat`, `refactor`) split from one file's mixed edits — with zero network and zero config. [`scripts/demo.sh`](scripts/demo.sh) materializes a throwaway repo, applies three unrelated edits to a single file, and replays aic's recorded split.

Want the **real** LLM doing the split? Configure a provider once (`aic setup`, or run a local [Ollama](https://ollama.com) with no key), then:

```sh
AIC_DEMO_LIVE=1 ./scripts/demo.sh
```

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
- **Interactive setup** — `aic setup` is menu-driven: pick the AI provider (backend, API key, base URL, model) or the per-commit confirmation toggle in any order, then save
- **Conventional Commits** — messages follow the [Conventional Commits v1.0.0](https://www.conventionalcommits.org/) spec
- **Configurable** — config file or per-run override

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

Reload your shell (`exec $SHELL`) and Tab completion is active. Supported: `bash`, `fish`, `zsh`, `nushell`. `bash` and `fish` are autoloaded; `zsh` needs its `site-functions` dir on `$fpath` (the entry is printed after install) — it's already there under Homebrew's own zsh but not macOS system zsh; `nushell` needs a `source` line in `config.nu` (printed after install).

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

## How aic compares

Most AI commit tools treat a **file** as the atomic unit. aic treats a **hunk** as the atomic unit — that is the only thing it claims to do that the others don't. Everything else, we concede honestly.

| Tool | AI messages | Hunk-level split | Conventional Commits | Multi-provider | Conflict resolution | Rust binary |
| --- | :---: | :---: | :---: | :---: | :---: | :---: |
| **aic** | ✅ | ✅ | ✅ | ✅ 12 | ✅ `aic resolve` | ✅ |
| [aicommits](https://github.com/Nutlope/aicommits) | ✅ | ❌ | ⚠️ generated, not enforced | ❌ OpenAI only | ❌ | ❌ Node |
| [opencommit](https://github.com/di-sukharev/opencommit) | ✅ | ❌ | ✅ | ✅ several | ❌ | ❌ Node |
| [commitizen](https://github.com/commitizen-tools/commitizen) | ❌ prompt only | ❌ | ✅ | — N/A | ❌ | ❌ Python |
| [gitmoji-cli](https://github.com/carloscuesta/gitmoji-cli) | ❌ prompt only | ❌ | ⚠️ emoji-focused | — N/A | ❌ | ❌ Node |
| [cz-cli](https://github.com/commitizen/cz-cli) | ❌ prompt only | ❌ | ✅ | — N/A | ❌ | ❌ Node |

**Where aic wins:** hunk-level atomic commits (the only tool here that does it), a single Rust binary, and built-in merge-conflict resolution.

**Where aic ties:** AI-generated messages and multi-provider support (opencommit is a strong peer here).

**Where aic concedes:** if you want a battle-tested *non-AI* Conventional Commits prompt wizard, `commitizen` / `cz-cli` are more mature and need no API key.

> Legend: ✅ supported · ❌ not supported · ⚠️ partial · — not applicable. Verified against each project's docs at the linked repos.

## Usage

| Command       | Description                                                                                                            |
| ------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `aic`         | Commit staged files with one message. If nothing is staged, batch-plan all unstaged changes into **hunk-level** atomic commits. |
| `aic resolve` | Resolve git merge conflicts via the LLM. Proposes per-file resolutions to review, then finalizes the merge.            |
| `aic setup`   | Menu-driven config: AI provider (backend, API key, base URL, model) and a pre-commit confirmation toggle, in any order.    |
| `aic list`    | Show resolved config: provider, model, and where each value comes from (config / default).                       |
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

The config file is the single source of truth: `~/.config/aic/config.toml`.
Environment variables are **not** read for provider settings — the values
`aic setup` saves are exactly what `aic` uses at runtime.

| Field      | Purpose                                        | Default          |
| ---------- | ---------------------------------------------- | ---------------- |
| `backend`  | Provider name                                  | `openai`         |
| `api_key`  | API key                                        | —                |
| `model`    | Model ID                                       | Provider default |
| `base_url` | Endpoint base URL (Ollama / OpenAI-compatible) | Provider default |

### Resolution order

For each of `backend`, `api_key`, `model`, and `base_url`:

1. Config file (`~/.config/aic/config.toml`)
2. Built-in default

### Pre-commit confirmation

By default `aic` drafts a message and commits immediately — you only see the
message *after* it lands. If you sign commits (GPG, the signing popup fires
before you see what you're signing) or run a local model whose drafts on large
commits need a human check, opt in:

```toml
confirm_before_commit = true
```

in `~/.config/aic/config.toml`, or toggle it during `aic setup`. With it on,
`aic` shows the drafted message (subject + body) and the files it would land,
then offers a four-option menu before each commit:

- **Commit** — land the commit as drafted
- **Re-generate** — re-run the model on the same diff for a fresh draft
- **Edit** — edit the full message in `$VISUAL`/`$EDITOR` (falls back to nano/vim/vi/emacs), then return to the menu
- **Abort** — end the run; nothing further commits

Abort in batch mode leaves already-committed batches in place and keeps the
rest in the working tree, recoverable by re-running `aic`.

### Supported providers

| Provider          | Default model                              | API key  | Base URL                                       |
| ----------------- | ------------------------------------------ | -------- | ---------------------------------------------- |
| OpenAI            | `gpt-5-mini`                               | required | built-in                                       |
| Anthropic         | `claude-haiku-4-5`                         | required | built-in                                       |
| Gemini            | `gemini-2.5-flash`                         | required | built-in                                       |
| DeepSeek          | `deepseek-v4-flash`                        | required | built-in                                       |
| Groq              | `llama-3.3-70b-versatile`                  | required | built-in                                       |
| xAI               | `grok-4.3`                                 | required | built-in                                       |
| Mistral           | `mistral-small-latest`                     | required | built-in                                       |
| OpenRouter        | _(model required)_                         | required | built-in                                       |
| Perplexity        | `sonar`                                    | required | built-in                                       |
| Together          | `meta-llama/Llama-3.3-70B-Instruct-Turbo`  | required | built-in                                       |
| Ollama            | `llama3.3`                                 | none     | optional (default `http://localhost:11434`)    |
| OpenAI-compatible | _(model required)_                         | optional | required                                       |

OpenRouter and the OpenAI-compatible provider have no default model — set `model` in config (run `aic setup`). The OpenAI-compatible provider also requires a `base_url` and routes through the OpenAI client against any server that speaks the OpenAI chat-completions API (LM Studio, vLLM, gateways).

Real per-provider smoke tests (one commit-message generation call each) live in `scripts/smoke-test-providers.sh` — export the provider key and run it to exercise any provider end to end.

## Resolving merge conflicts

Run `aic resolve` when your repo is mid-merge. It reads each conflicted file, proposes a marker-free resolution, shows you the diff, and asks `apply?` per file. Approve the ones you trust; the rest stay untouched. When nothing is left unmerged, it runs the merge's `--continue` for you.

You can also run plain `aic` in a conflicted repo — it notices and offers to hand off to resolve, and a commit guard blocks any commit that still carries conflict markers.

**v1 limits:** `aic resolve` handles conflicted **merge** state — a rebase or `am` in flight is detected and refused. Binary, oversized, and delete/modify conflicts are skipped with a reason for you to resolve by hand. Finalize is all-or-nothing: `--continue` blocks on any unmerged path, and the hand-off tells you exactly what's left.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) — branch rules, commit style, and what to expect from review. (简体中文: [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md))

## License

[MIT](https://github.com/CaicoLeung/aic/blob/main/LICENSE)
