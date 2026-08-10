# aic

> **简体中文:** [README.zh-CN.md](./README.zh-CN.md)

AI commit messages that are **actually atomic** — and work with your existing tools, **no API key needed**.

🌐 **Website:** <https://caicoleung.github.io/aic-web/>

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml)
[![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

![aic splits one file into three atomic commits](docs/demo.gif)

---

## ✨ Hunk-level commits, not file-level

Most AI commit tools treat a **file** as the atomic unit. aic treats a **hunk** — a single contiguous change — as the unit.

Edit one file in three unrelated ways and `aic` produces three clean commits. No manual `git add -p`, no mixed-concern history.

```
src/auth.rs  (one file, three changes)

  Other tools:  1 commit  "update src/auth.rs"        ❌ mixed concerns
  aic:          3 commits, one per logical change      ✅

    ├─ hunk 1  →  fix(auth): correct token expiry check
    ├─ hunk 2  →  feat(auth): add OAuth2 login provider
    └─ hunk 3  →  style(auth): tidy imports
```

Run `aic` with **nothing staged** and it auto-detects every unstaged hunk, groups them by concern, and commits each group separately — with the model's reasoning streamed live as it plans the split.

Nothing gets lost or double-committed: every hunk lands in exactly one commit, or the plan is rejected.

---

## Quick Start

```sh
# 1. Install (macOS / Linux)
curl --proto '=https' --tlsv1.2 -sSfL \
  https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.sh | sh

# 2. Configure — pick an API provider OR a CLI agent
aic setup

# 3. Commit — no need to stage, just run aic
aic
# → 3 atomic commits from your working tree
```

> 💡 **No API key?** Skip the provider setup — use [Claude Code, Codex, pi, or opencode](#-no-api-key-use-your-ai-agent) instead. aic reuses their auth.

> **Windows (PowerShell):** `irm https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.ps1 | iex`

---

## 🔑 No API key? Use your AI agent

Already have [Claude Code](https://docs.anthropic.com/claude/docs/claude-code), [OpenAI Codex](https://github.com/openai/codex), [pi](https://pi.dev), or [opencode](https://opencode.ai) installed and authenticated? aic can drive it in **headless mode** — no API key needed.

```sh
aic setup    # → select "CLI agent" → pick your tool → done
```

Or edit `~/.config/aic/config.toml` directly:

```toml
backend_kind = "cli"
command = "claude"
args = ["-p", "{prompt}", "--output-format", "stream-json", "--include-partial-messages"]
```

aic sends one prompt and reads the answer — it never runs the agent in tool-use mode. Each preset pins itself to read-only or text-only, so an injected instruction can't touch your working tree. Both backends' fields can coexist in the config; `backend_kind` selects the active one.

See [CLI-agent presets](#cli-agent-presets) for Codex, pi, and opencode.

---

## Features

- **Hunk-level splitting** — one file, many concerns? Splits per-hunk into atomic commits, fully non-interactive
- **Two backends** — API provider (12+ supported) or CLI agent (Claude Code, Codex, pi, opencode — no API key)
- **Merge conflict resolution** — `aic resolve` proposes per-file resolutions you review, then finalizes the merge
- **Live reasoning** — watch the model think as it decides the split
- **Conventional Commits** — messages follow the [v1.0.0 spec](https://www.conventionalcommits.org/)
- **Interactive setup** — `aic setup` is menu-driven; `aic use` switches between saved provider profiles

## Installation

| Method | Command |
|--------|---------|
| **Binary** (macOS/Linux) | `curl -sSfL https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.sh \| sh` |
| **Homebrew** | `brew tap CaicoLeung/aic && brew install aic` |
| **Source** | `git clone … && cargo build --release` → `target/release/aic` |

Shell completions: `aic completion` (bash, fish, zsh, nushell).

## Usage

| Command | Description |
|---------|-------------|
| `aic` | Commit staged files. If nothing is staged, auto-split all unstaged changes into hunk-level atomic commits. |
| `aic resolve` | Resolve git merge conflicts via the LLM. Review each file, then finalize. |
| `aic setup` | Menu-driven config: API provider, CLI agent, or pre-commit confirmation. |
| `aic use <provider>` | Switch to a provider already configured via `aic setup`. |
| `aic list` | Show resolved config and where each value comes from. |
| `aic update` | Update to the latest release. |
| `aic completion` | Install shell completions. |

## Configuration

Config lives at `~/.config/aic/config.toml` — the single source of truth (env vars are **not** read).

**Two backends**, toggled by `backend_kind`:

| `backend_kind` | What it uses | Key fields |
|----------------|-------------|------------|
| `"api"` (default) | An LLM API provider over HTTP | `backend`, `api_key`, `model`, `base_url` |
| `"cli"` | A local coding-agent CLI | `command`, `args`, `timeout_secs` |

> ⚠️ **Want to review before committing?** Set `confirm_before_commit = true` for a **Commit / Re-generate / Edit / Abort** menu before each commit.

Full reference: [provider table](#supported-providers) · [CLI-agent presets](#cli-agent-presets) · [config fields](#config-fields).

---

## Resolving merge conflicts

Run `aic resolve` in a repo mid-merge. It reads each conflicted file, proposes a marker-free resolution, shows you the diff, and asks `apply?` per file. When nothing's left unmerged, it runs the merge's `--continue` for you. Plain `aic` in a conflicted repo notices and offers to hand off.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) — branch rules, commit style, review expectations. (简体中文: [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md))

## License

[MIT](https://github.com/CaicoLeung/aic/blob/main/LICENSE)

---

<details>
<summary><b>📋 Full configuration reference</b></summary>

### Supported providers

| Provider | Default model | API key | Base URL |
|----------|--------------|---------|----------|
| OpenAI | `gpt-5-mini` | required | built-in |
| Anthropic | `claude-haiku-4-5` | required | built-in |
| Gemini | `gemini-2.5-flash` | required | built-in |
| DeepSeek | `deepseek-v4-flash` | required | built-in |
| Groq | `llama-3.3-70b-versatile` | required | built-in |
| xAI | `grok-4.3` | required | built-in |
| Mistral | `mistral-small-latest` | required | built-in |
| OpenRouter | _(set `model`)_ | required | built-in |
| Perplexity | `sonar` | required | built-in |
| Together | `meta-llama/Llama-3.3-70B-Instruct-Turbo` | required | built-in |
| Ollama | `llama3.3` | none | `http://localhost:11434` |
| OpenAI-compatible | _(set `model`)_ | optional | required |

OpenRouter and the OpenAI-compatible provider have no default model — set `model` in config. The OpenAI-compatible provider requires `base_url` and routes through the OpenAI client (LM Studio, vLLM, gateways).

### CLI-agent presets

Each preset ships a dedicated decoder for its CLI's stdout envelope, so aic can stream reasoning where the CLI exposes it and cleanly extract the answer.

```toml
# OpenAI Codex — exec --json, read-only sandbox
backend_kind = "cli"
command = "codex"
args = ["exec", "--json", "-s", "read-only", "{prompt}"]
```

```toml
# pi — --no-tools disables all tools; --mode json streams reasoning + answer
backend_kind = "cli"
command = "pi"
args = ["--no-tools", "--mode", "json", "-p", "{prompt}"]
```

```toml
# opencode — run --format json; reuses its own auth (cursor oauth / provider keys)
backend_kind = "cli"
command = "opencode"
args = ["run", "--format", "json", "{prompt}"]
```

The CLI must already be installed and logged in — aic does not install or authenticate it.

See [ADR 0010](docs/adr/0010-cli-agent-backend.md) for the backend design and [ADR 0011](docs/adr/0011-explicit-backend-discriminator.md) for the `backend_kind` discriminator.

### Config fields

| Field | Purpose | Default |
|-------|---------|---------|
| `backend_kind` | `"api"` or `"cli"` | `"api"` |
| `backend` | Provider name (API backend) | `openai` |
| `api_key` | API key (API backend) | — |
| `model` | Model ID (API backend) | Provider default |
| `base_url` | Endpoint URL (Ollama / OpenAI-compatible) | Provider default |
| `command` | CLI command (CLI backend) | — |
| `args` | Argv template, `{prompt}` is replaced | `["{prompt}"]` |
| `timeout_secs` | Per-call idle timeout (CLI backend) | `240` (streaming) / `600` (batch) |
| `confirm_before_commit` | Show review menu before each commit | `false` |

</details>
