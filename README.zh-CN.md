# aic

> **English:** [README.md](./README.md)

AI 驱动的 git commit 工具，写出 **真正原子化** 的提交信息 —— 而且可以直接复用你已有的 AI 工具，**无需 API key**。

🌐 **官网:** <https://caicoleung.github.io/aic-web/>

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml)
[![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

![aic 把一个文件拆成三个原子提交](https://vhs.charm.sh/vhs-78XLWnaEdLvjDqeCkETQVh.gif)

---

## ✨ hunk 级别的提交，而非 file 级别

多数 AI commit 工具把 **file** 当作原子单位。aic 则把 **hunk**（一段连续的代码改动）当作原子单位。

在一个文件里做了三处互不相关的改动，`aic` 会产出三个干净的提交 —— 无需手动 `git add -p`，也不会出现 concern 混杂的历史。

```
src/auth.rs  （一个文件，三处改动）

  其他工具:  1 个 commit  "update src/auth.rs"        ❌ concern 混杂
  aic:       3 个 commit，每个对应一处逻辑改动          ✅

    ├─ hunk 1  →  fix(auth): correct token expiry check
    ├─ hunk 2  →  feat(auth): add OAuth2 login provider
    └─ hunk 3  →  style(auth): tidy imports
```

什么都不 stage 直接运行 `aic`，它会自动检测所有未暂存的 hunk，按逻辑 concern 分组，逐组提交 —— 分组时模型的推理过程实时流式输出。

不会丢失或重复提交：每个 hunk 恰好落在一个 commit 中，否则整个 plan 会被拒绝。

---

## 快速上手

```sh
# 1. 安装（macOS / Linux）
curl --proto '=https' --tlsv1.2 -sSfL \
  https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.sh | sh

# 2. 配置 —— 选择 API provider 或 CLI agent
aic setup

# 3. 提交 —— 无需手动 stage，直接运行 aic
aic
# → 从你的工作区生成 3 个原子提交
```

> 💡 **没有 API key？** 跳过 provider 配置 —— 直接使用 [Claude Code、Codex、pi、opencode 以及另外 7 个 CLI agent](#-没有-api-key复用你的-ai-agent)，aic 会复用它们的登录认证。

> **Windows (PowerShell):** `irm https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.ps1 | iex`

---

## 🔑 没有 API key？复用你的 AI agent

已经安装并登录了 [Claude Code](https://docs.anthropic.com/claude/docs/claude-code)、[OpenAI Codex](https://github.com/openai/codex)、[pi](https://pi.dev) 或 [opencode](https://opencode.ai)？aic 可以在 **headless 模式** 下驱动它 —— 无需 API key。

同样支持：**oh-my-pi**（`omp`）、**Gemini**（`gemini`）、**Cursor**（`cursor-agent`）、**Windsurf**（`devin` —— Windsurf 已更名为 Devin Desktop）、**GitHub Copilot**（`copilot`）、**Trae**（`traecli`）和 **Qwen Code**（`qwen`）。

```sh
aic setup    # → 选择 "CLI agent" → 选你的工具 → 完成
```

或者直接编辑 `~/.config/aic/config.toml`：

```toml
backend_kind = "cli"
command = "claude"
args = ["-p", "{prompt}", "--output-format", "stream-json", "--include-partial-messages"]
```

aic 只发送一条 prompt 并读取回答 —— 绝不在 tool-use 模式下运行 agent。需要时预设会显式锁定权限（codex 的只读沙箱、pi 的 `--no-tools`）；其余预设依赖无头 print 模式 —— 没有 TTY，需审批的工具无法运行 —— 因此注入的指令无法触碰你的工作区。两个 backend 的字段可以共存于配置文件中；`backend_kind` 决定哪个生效。

其他预设见 [CLI-agent 预设](#cli-agent-预设)。

---

## 功能特性

- **Hunk 级别拆分** —— 一个文件、多种 concern？按 hunk 拆成多个原子提交，完全非交互
- **两种 backend** —— API provider（支持 12+ 家）或 CLI agent（11 个预设：claude、codex、pi、opencode、omp、gemini、cursor、windsurf、copilot、trae、qwen —— 无需 API key）
- **Merge 冲突解决** —— `aic resolve` 逐文件给出方案供你审核，然后完成 merge
- **实时推理** —— 观看模型思考拆分方案的全过程
- **Conventional Commits** —— message 遵循 [v1.0.0 规范](https://www.conventionalcommits.org/)
- **交互式配置** —— `aic setup` 菜单驱动；`aic use` 在已保存的 provider 与 CLI agent（claude、codex、pi、opencode、omp、gemini、cursor、windsurf、copilot、trae、qwen）之间切换

## 安装

| 方式 | 命令 |
|------|------|
| **二进制**（macOS/Linux） | `curl -sSfL https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.sh \| sh` |
| **Homebrew** | `brew tap CaicoLeung/aic && brew install aic` |
| **源码编译** | `git clone … && cargo build --release` → `target/release/aic` |

Shell 补全：`aic completion`（bash、fish、zsh、nushell）。

## 命令一览

| 命令 | 说明 |
|------|------|
| `aic` | 提交已 stage 的文件。若无 stage 内容，自动将所有未暂存改动拆分为 hunk 级别的原子提交。 |
| `aic resolve` | 通过 LLM 解决 git merge 冲突。逐文件审核后完成 merge。 |
| `aic setup` | 菜单驱动配置：API provider、CLI agent、或提交前确认。 |
| `aic use <name>` | 切换到已通过 `aic setup` 配置过的 provider，或切换到 CLI agent（claude、codex、pi、opencode、omp、gemini、cursor、windsurf、copilot、trae、qwen）。 |
| `aic list` | 展示已 resolve 的 config 及每个值的来源。 |
| `aic update` | 更新到最新版本。 |
| `aic completion` | 安装 shell 补全。 |

## 配置

配置文件位于 `~/.config/aic/config.toml` —— 唯一真相来源（**不**读取环境变量）。

**两种 backend**，通过 `backend_kind` 切换：

| `backend_kind` | 使用什么 | 关键字段 |
|----------------|---------|---------|
| `"api"`（默认） | 通过 HTTP 调用 LLM API provider | `backend`、`api_key`、`model`、`base_url` |
| `"cli"` | 本地 coding-agent CLI | `command`、`args`、`timeout_secs` |

> ⚠️ **想在提交前审核？** 设置 `confirm_before_commit = true`，每次提交前弹出 **提交 / 重新生成 / 编辑 / 中止** 菜单。

完整参考：[provider 表](#支持的-provider) · [CLI-agent 预设](#cli-agent-预设) · [配置字段](#配置字段)。

---

## 解决 merge 冲突

![aic resolve 提出解决方案并完成 merge](https://vhs.charm.sh/vhs-3ZyDoDLS0ZghlrrVsJa70w.gif)

在处于 merge 中的 repo 运行 `aic resolve`。它会读取每个冲突文件，提出不含 marker 的解决方案，展示 diff，并逐文件询问 `apply?`。当没有未 merge 的内容残留时，它会替你执行 merge 的 `--continue`。在冲突 repo 中直接运行 `aic` 也会察觉并提议移交给 resolve。

## 贡献

详见 [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md) — 分支规则、提交风格、评审预期。(English: [CONTRIBUTING.md](./CONTRIBUTING.md))

## 许可证

[MIT](https://github.com/CaicoLeung/aic/blob/main/LICENSE)

---

<details>
<summary><b>📋 完整配置参考</b></summary>

### 支持的 provider

| Provider | 默认 model | API key | Base URL |
|----------|-----------|---------|----------|
| OpenAI | `gpt-5-mini` | 必需 | 内置 |
| Anthropic | `claude-haiku-4-5` | 必需 | 内置 |
| Gemini | `gemini-2.5-flash` | 必需 | 内置 |
| DeepSeek | `deepseek-v4-flash` | 必需 | 内置 |
| Groq | `llama-3.3-70b-versatile` | 必需 | 内置 |
| xAI | `grok-4.3` | 必需 | 内置 |
| Mistral | `mistral-small-latest` | 必需 | 内置 |
| OpenRouter | _(需指定 `model`)_ | 必需 | 内置 |
| Perplexity | `sonar` | 必需 | 内置 |
| Together | `meta-llama/Llama-3.3-70B-Instruct-Turbo` | 必需 | 内置 |
| Ollama | `llama3.3` | 无 | `http://localhost:11434` |
| OpenAI-compatible | _(需指定 `model`)_ | 可选 | 必需 |

OpenRouter 和 OpenAI-compatible provider 没有默认 model —— 在 config 中设置 `model`。OpenAI-compatible provider 还需要 `base_url`，通过 OpenAI client 路由（LM Studio、vLLM、各类 gateway）。

### CLI-agent 预设

带可解码 stdout 封装的预设（claude、codex、pi、opencode、omp）配有解码器 —— omp 作为 pi 的复刻直接复用 pi 的解码器 —— aic 能在 CLI 支持的情况下流式输出推理过程并干净地提取回答；其余预设为纯打印模式 —— stdout 即回答。

```toml
# OpenAI Codex — exec --json，只读沙箱
backend_kind = "cli"
command = "codex"
args = ["exec", "--json", "-s", "read-only", "{prompt}"]
```

```toml
# pi — --no-tools 禁用所有工具；--mode json 流式输出推理 + 回答
backend_kind = "cli"
command = "pi"
args = ["--no-tools", "--mode", "json", "-p", "{prompt}"]
```

```toml
# opencode — run --format json；复用自身认证（cursor oauth / provider keys）
backend_kind = "cli"
command = "opencode"
args = ["run", "--format", "json", "{prompt}"]
```

其余预设 —— 均为单次打印模式（`-p`），回答输出到 stdout：

| 预设 | 命令 | 说明 |
|------|------|------|
| `omp` | `omp --mode json {prompt}` | pi 分支；pi 同构 NDJSON，带推理流 |
| `gemini` | `gemini -p {prompt}` | 在 `aic use` 中遮蔽 `gemini` provider 名 —— Google API 仍可通过 `aic use google` 使用 |
| `cursor` | `cursor-agent -p {prompt}` | 不带 `--trust` → 以未信任模式运行（禁用写入） |
| `windsurf` | `devin -p {prompt}` | Windsurf 已更名 Devin Desktop；`devin auth login` 登录 |
| `copilot` | `copilot -p {prompt}` | 工具调用需要 headless 无法给出的交互式审批 |
| `trae` | `traecli -p {prompt}` | 非只读工具被权限提示门控 |
| `qwen` | `qwen -p {prompt}` | Qwen Code，gemini-cli 血统 |

该 CLI 必须已安装并登录 —— aic 不负责安装或认证。

详见 [ADR 0010](docs/adr/0010-cli-agent-backend.md)（backend 设计）和 [ADR 0011](docs/adr/0011-explicit-backend-discriminator.md)（`backend_kind` 判别字段）。

### 配置字段

| 字段 | 用途 | 默认值 |
|------|------|--------|
| `backend_kind` | `"api"` 或 `"cli"` | `"api"` |
| `backend` | provider 名称（API backend） | `openai` |
| `api_key` | API key（API backend） | — |
| `model` | model ID（API backend） | Provider default |
| `base_url` | endpoint URL（Ollama / OpenAI-compatible） | Provider default |
| `command` | CLI 命令（CLI backend） | — |
| `args` | Argv 模板，`{prompt}` 会被替换 | `["{prompt}"]` |
| `timeout_secs` | 每次调用空闲超时（CLI backend） | `240`（流式）/ `600`（批量） |
| `confirm_before_commit` | 提交前显示审核菜单 | `false` |

</details>
