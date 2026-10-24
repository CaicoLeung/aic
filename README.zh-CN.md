# aic

> **English:** [README.md](./README.md)

aic 把你杂乱的工作区变成一串干净的原子化 commit —— 由你已有的 AI 驱动。

🌐 **官网:** <https://caicoleung.github.io/aic-web/>

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml) [![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

![aic 把一个文件的改动拆成原子提交](https://vhs.charm.sh/vhs-2lPY6yY1uBpk7fWCgd7ypH.gif)

*无需 stage、无需 `git add -p` —— 一个文件里的三处无关改动，变成三个干净的原子 commit。*

## 快速上手

```sh
# 1. 安装（macOS / Linux）
curl --proto '=https' --tlsv1.2 -sSfL \
  https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.sh | sh

# 2. 配置 —— 选择 API provider 或 CLI agent
aic setup

# 3. 提交 —— 无需手动 stage，直接运行 aic
aic
```

> 💡 **没有 API key？** 跳过 provider 配置 —— 直接使用 [Claude Code、Codex、pi、opencode 以及另外 7 个 CLI agent](#-没有-api-key复用你的-ai-agent)，aic 会复用它们的登录认证。

> **Windows (PowerShell):** `irm https://github.com/CaicoLeung/aic/releases/latest/download/aic-installer.ps1 | iex`

## ✨ hunk 级别的提交，而非 file 级别

**hunk**（一段连续的代码改动）是 aic 的原子单位 —— 每个逻辑改动单独提交，一个文件里的三处编辑就是三个干净的 commit。其他工具按整个 file 提交；aic 不会再写出 "update auth.rs" 这种 concern 混杂的提交，也无需手动 `git add -p`。

```text
src/auth.rs —— 一个文件，三处改动

  ├─ hunk 1  →  fix(auth): correct token expiry check
  ├─ hunk 2  →  feat(auth): add OAuth2 login provider
  └─ hunk 3  →  style(auth): tidy imports
```

什么都不 stage 直接运行 `aic`，它会自动检测所有未暂存的 hunk，按逻辑 concern 分组，逐组提交 —— 分组时模型的推理过程实时流式输出。不会丢失或重复提交：每个 hunk 恰好落在一个 commit 中，否则整个 plan 会被拒绝。

## 🔑 没有 API key？复用你的 AI agent

已经安装并登录了 [Claude Code](https://docs.anthropic.com/claude/docs/claude-code)、[OpenAI Codex](https://github.com/openai/codex)、[pi](https://pi.dev) 或 [opencode](https://opencode.ai)？aic 会在 **headless 模式**下驱动它并复用其认证 —— 无需 API key。同样支持：**oh-my-pi**（`omp`）、**Gemini**（`gemini`）、**Cursor**（`cursor-agent`）、**Windsurf**（`devin` —— Windsurf 已更名为 Devin Desktop）、**GitHub Copilot**（`copilot`）、**Trae**（`traecli`）和 **Qwen Code**（`qwen`）。

运行 `aic setup`，选择 **CLI agent**，挑你的工具 —— 完成。aic 只发送一条 prompt 并读取回答：绝不在 tool-use 模式下运行 agent，注入的指令无法触碰你的工作区。每个预设的确切 flag 与可直接粘贴的 TOML 见 [CLI-agent 预设](#cli-agent-预设)。

## 功能特性

- **hunk 级原子提交** —— 一个文件、三处无关改动？三个 commit —— 而不是一个混杂的 "update auth.rs"。全程非交互，模型推理实时流式输出。
- **无需 API key** —— 直接驱动本机已登录的 CLI agent（Claude Code、Codex、pi、opencode 等共 11 个），也可从 12 家 API provider 中任选。
- **Conventional Commits** —— 每条 message 遵循 [v1.0.0 规范](https://www.conventionalcommits.org/)，作用于它所描述的那个逻辑改动。
- **Merge 冲突解决** —— `aic resolve` 逐文件给出方案供你审核，然后完成 merge。
- **一键撤销** —— `aic undo` 重置上一次 Run 的提交，全部改动原样回到工作区（未暂存）。
- **密钥拦截门** —— diff 中出现密钥特征内容时，Run 会在**发送给 LLM 之前**拒绝执行；`--no-redact` 可知情放行。

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
| `aic` | 提交已 stage 的内容 —— 若无 stage 内容，则把所有未暂存改动拆成干净的原子提交。 |
| `aic --hint "<文本>"` | 同上，但向本次 Run 的两个提示词（batch 计划与提交信息）各追加一条一次性指令 —— 如 `aic --hint "closes #78"` 或 `aic --hint "拆得更细"`。 |
| `aic undo` | 撤销上一次 Run：重置回 Run 开始前的状态，全部改动回到工作区（未暂存）。确认时列明将重置哪些提交（含 Run 之后的）；若期间历史被改写则拒绝执行。 |
| `aic resolve` | 通过 LLM 解决 git merge 冲突。逐文件审核后完成 merge。 |
| `aic setup` / `aic use <name>` | 菜单驱动配置：API provider、CLI agent、或提交前确认；`aic use` 在已保存的 provider 与 CLI agent 之间切换。 |

密钥拦截门、`--no-redact`，以及其余命令（`aic list`、`aic update`、`aic completion`）：[其他命令](#其他命令)。

## 配置

配置文件位于 `~/.config/aic/config.toml` —— 唯一真相来源（**不**读取环境变量）。两种 backend 通过 `backend_kind` 切换：经 HTTP 调用的 LLM API provider，或本地 coding-agent CLI。两套字段可在文件中共存；`backend_kind` 决定哪个生效。

完整参考：[支持的 provider](#支持的-provider) · [CLI-agent 预设](#cli-agent-预设) · [配置字段](#配置字段) · [其他命令](#其他命令)。

## 解决 merge 冲突

![aic resolve 提出解决方案并完成 merge](https://vhs.charm.sh/vhs-7qCGKxgjJXzbXVdwa5bmx2.gif)

*merge 冲突中，`aic resolve` 逐文件提出不含 marker 的解决方案 —— 你审核 diff、逐个确认，它来完成 merge。*

在处于 merge 中的 repo 运行 `aic resolve`：它读取每个冲突文件、提出解决方案、展示 diff、逐文件询问 `apply?`，当没有未 merge 的内容残留时，替你执行 merge 的 `--continue`。在冲突 repo 中直接运行 `aic` 也会察觉并提议移交给 resolve。

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
# Claude Code — headless 打印模式，流式 JSON
backend_kind = "cli"
command = "claude"
args = ["-p", "{prompt}", "--output-format", "stream-json", "--include-partial-messages"]
```

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
| `confirm_before_commit` | 每次提交落库前显示 Commit confirmation 菜单（提交 / 重新生成 / 编辑 / 中止） | `false` |
| `review_batch_plan` | 起草前显示 Batch Review 菜单（可合并 Batch） | `false` |

### 其他命令

| 命令 | 说明 |
|------|------|
| `aic --no-redact` | 同 `aic`，但跳过密钥拦截门：默认情况下，diff 中含密钥特征内容（云厂商 key、token、私钥块）的 Run 会在发送给 LLM 之前拒绝，并指明文件与模式类型。本 flag 将 diff 原样发送，仅对本次 Run 生效。（冲突仓库请用 `aic resolve` 自己的 `--no-redact`。） |
| `aic resolve --no-redact` | 同 `aic resolve`，但跳过密钥拦截门：默认情况下，冲突文件内容含密钥特征内容时会在发送给 LLM 之前拒绝，并指明文件与模式类型。本 flag 将整个文件内容原样发送，仅对本次 resolve 生效。 |
| `aic list` | 展示已 resolve 的 config 及每个值的来源。 |
| `aic update` | 更新到最新版本。 |
| `aic completion` | 安装 shell 补全。 |

</details>
