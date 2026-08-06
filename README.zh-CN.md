# aic

> **English:** [README.md](./README.md)

AI 驱动的 git commit 工具，帮你写 Conventional Commit message —— 并把你的改动拆成 **hunk 级别**的 atomic commit，而不是 file 级别。

🌐 **官网:** <https://caicoleung.github.io/aic-web/>

[![CI](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/ci.yml)
[![Release](https://github.com/CaicoLeung/aic/actions/workflows/release.yml/badge.svg)](https://github.com/CaicoLeung/aic/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Changelog](https://img.shields.io/badge/changelog-CHANGELOG.md-blue)](./CHANGELOG.md)

---

## ✨ 核心亮点：hunk 级别的 commit，而非 file 级别

多数 AI commit 工具把 **file** 当作 atomic 单位。aic 则把 **hunk**（一段连续的代码改动）当作 atomic 单位。

在一个文件里做了三处互不相关的改动，`aic` 会产出三个干净、atomic 的 commit —— 无需手动 `git add -p`，也不会出现 concern 混杂的 commit。

```
你在同一个文件里做了三处互不相关的改动：

  src/auth.rs
    ├─ hunk 1  修复 token-expiry 检查    →  fix(auth): correct token expiry check
    ├─ hunk 2  新增 OAuth2 login provider →  feat(auth): add OAuth2 login provider
    └─ hunk 3  整理 import               →  style(auth): tidy imports

其他工具:  1 个 commit  "update src/auth.rs"     ❌ concern 混杂，history 模糊
aic:       3 个 commit，每个对应一处逻辑改动      ✅
```

**为什么安全：**

- **Exact-partition 校验** —— 每个 hunk 精确分配到恰好一个 commit，无重叠、无遗漏，因此不会丢失或重复 commit。只要有任何一个 hunk 缺失或越界，整个 plan 就会被拒绝。
- **Context-aware 的 staging** —— 选中的 hunk 会重建为 patch，再用 `git apply --cached` stage；该命令会依据周围的 context 行重新定位每个 hunk，因此即便前一个 commit 改变了行号，hunk 依然能正确落地。
- **Live reasoning** —— aic 会在 model 决定拆分方案时实时流式输出它的思考过程，让你在 commit 落地前看清每个 hunk *为何* 被归到一起。

## Features

- **Hunk 级别的批量拆分** —— 一个文件、多种 concern？aic 按 hunk 拆成多个 atomic commit（`git add -p` 风格，完全非交互）
- **Multi-provider** —— OpenAI、Anthropic、Gemini、DeepSeek、Groq、xAI、Mistral、OpenRouter、Perplexity、Together、Ollama，以及任何 OpenAI-compatible server
- **Conflict resolution** —— 正处于 merge 中？`aic resolve` 会逐文件给出解决方案供你 review 和批准，然后完成 merge
- **Interactive setup** —— `aic setup` 引导你完成 provider、API key、model 的选择，并可开启提交前确认开关
- **Conventional Commits** —— message 遵循 [Conventional Commits v1.0.0](https://www.conventionalcommits.org/) 规范
- **可配置** —— config 文件、环境变量，或单次运行 override

## Installation

### Binary（推荐）

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

用 `brew upgrade aic` 更新。Homebrew 安装会被自动识别，因此 `aic update` 会直接引导你使用 brew，不会改动任何东西。

### Build from source

```sh
git clone https://github.com/CaicoLeung/aic.git
cd aic
cargo build --release
# binary 位于 target/release/aic
```

### Shell 补全（Tab 自动补全）

一条命令即可安装补全 —— `aic` 会交互式让你选择 shell（默认高亮 `$SHELL` 探测到的），并把脚本写入约定位置：

```sh
aic completion            # 交互式选择 shell 并安装
```

重载 shell（`exec $SHELL`）后 Tab 补全即生效。支持：`bash`、`fish`、`zsh`、`nushell`。`bash` 和 `fish` 重载即自动生效；`zsh` 需将 `site-functions` 目录加入 `$fpath`（安装后会打印该条目）——Homebrew 自带的 zsh 已包含该目录，而 macOS 系统 zsh 则不含；`nushell` 需在 `config.nu` 加一行 `source`（安装后会打印）。

## Quick Start

```sh
# 1. 配置你的 LLM provider
aic setup

# 2. stage 一些文件并 commit
git add src/main.rs
aic
# → feat: add CLI argument parsing
#   Created commit abc1234

# 3. 或者什么都不 stage 直接运行 —— aic 会把你的 workdir 改动
#    自动拆分为 hunk 级别的 atomic commit
aic
```

## Usage

| 命令           | 说明                                                                                                                  |
| -------------- | --------------------------------------------------------------------------------------------------------------------- |
| `aic`          | 用一条 message commit 已 stage 的文件。若没有 stage 任何内容，则把所有未 stage 的改动批量规划为 **hunk 级别**的 atomic commit。 |
| `aic resolve`  | 通过 LLM 解决 git merge conflict。逐文件给出方案供 review，然后完成 merge。                                            |
| `aic setup`    | Interactive 向导，选择 provider、输入 API key、选择 model；也可切换提交前确认。                                          |
| `aic list`     | 展示已 resolve 的 config：provider、model，以及每个值来自哪里（env / config / default）。                              |
| `aic update`   | 从 GitHub Releases 把 aic 更新到最新版本。                                                                             |

## How It Works

```
aic
  ├─ 有 staged 文件？ → diff staged 文件 → LLM 写 message → commit
  └─ 没有 stage？     → diff workdir → LLM 把每个 hunk 划分到不同 batch：
        对每个 batch（model 思考时实时流式输出 reasoning）：
          通过 `git apply --cached` stage 其 hunk（按 context 重新定位）
          → LLM 写 message → commit

aic resolve
  └─ 处于 conflict 的 repo？ → 对每个冲突文件：
        LLM 提出解决方案 → 校验 marker（重试一次）
        → review diff → apply? [y/n] → git add
        → 全部解决后完成 merge（git --continue）
```

所有 commit message 遵循 Conventional Commits（`feat:`、`fix:`、`refactor:` 等），可带可选 body。

### Hunk splitter 内部流程

1. **Diff 一次** —— aic 取出每个文件 workdir 对比 HEAD 的 diff，并给其 hunk 编号（1、2、3 ……）。
2. **Partition** —— model 把每个 hunk index 分配到恰好一个 batch，按逻辑 concern 分组。一个 batch 可以携带来自多个文件的 hunk；同一个文件的 hunk 也可以分散到多个 batch。
3. **Validate** —— 校验该 plan 是否为 *exact partition*：每个 hunk 恰好覆盖一次，无重叠，无越界或未知文件引用。校验失败则不会产生任何 commit。
4. **Stage & commit** —— 对每个 batch，把选中的 hunk 重建为 patch，用 `git apply --cached` apply；该命令会按 context 重新定位 hunk，因此即便前一个 commit 改变了行号也能正确落地。随后生成 message 并创建 commit。

## Configuration

Config 文件：`~/.config/aic/config.toml`

### 环境变量

| 变量               | 用途                                            | 默认值           |
| ------------------ | ----------------------------------------------- | ---------------- |
| `LLM_BACKEND`      | provider 名称                                   | `openai`         |
| `LLM_API_KEY`      | API key（回退到 provider 专属变量）             | —                |
| `LLM_MODEL`        | model ID override                               | Provider default |
| `LLM_BASE_URL`     | endpoint base URL（Ollama / OpenAI-compatible） | Provider default |
| `AIC_SYSTEM_PROMPT`| override commit message 的 system prompt        | Built-in prompt  |

Provider 专属的 API key 环境变量（`OPENAI_API_KEY`、`ANTHROPIC_API_KEY` 等）同样会被识别。

### Resolution order

对 `backend`、`api_key`、`model`、`base_url` 每一项：

1. 通用环境变量（`LLM_BACKEND`、`LLM_API_KEY`、`LLM_MODEL`）
2. Provider 专属环境变量（仅 API key）
3. Config 文件（`~/.config/aic/config.toml`）
4. Built-in default

### 提交前确认

默认情况下 `aic` 生成 message 后立即提交——你只能在提交*之后*看到 message。如果你用 GPG 签名提交（签名弹窗在你能看到要签什么之前就会触发），或者你本地跑的是较弱模型、大提交的草稿需要人工检查，可以开启该选项：

```toml
confirm_before_commit = true
```

写入 `~/.config/aic/config.toml`，或在 `aic setup` 中切换。开启后，`aic` 会在每次提交前展示草拟的 message（subject + body）以及将落地的文件，然后提供四个选项的菜单：

- **Commit** — 按草稿提交
- **Re-generate** — 对同一 diff 重新生成一份草稿
- **Edit** — 编辑完整 message（终端内联编辑器；非 TTY 时用 `$VISUAL`/`$EDITOR` 打开临时文件），然后回到菜单
- **Abort** — 结束本次运行，不再提交

Batch 模式下 Abort 后，已提交的 batch 保持不变，其余更改留在工作区，重新运行 `aic` 即可继续。

### 支持的 provider

| Provider          | 默认 model                                 | Env key                                                       |
| ----------------- | ------------------------------------------ | ------------------------------------------------------------- |
| OpenAI            | `gpt-5-mini`                               | `OPENAI_API_KEY`                                              |
| Anthropic         | `claude-haiku-4-5`                         | `ANTHROPIC_API_KEY`                                           |
| Gemini            | `gemini-2.5-flash`                         | `GEMINI_API_KEY`                                              |
| DeepSeek          | `deepseek-v4-flash`                        | `DEEPSEEK_API_KEY`                                            |
| Groq              | `llama-3.3-70b-versatile`                  | `GROQ_API_KEY`                                                |
| xAI               | `grok-4.3`                                 | `XAI_API_KEY`                                                 |
| Mistral           | `mistral-small-latest`                     | `MISTRAL_API_KEY`                                             |
| OpenRouter        | _(必须指定 model)_                         | `OPENROUTER_API_KEY`                                          |
| Perplexity        | `sonar`                                    | `PERPLEXITY_API_KEY`                                          |
| Together          | `meta-llama/Llama-3.3-70B-Instruct-Turbo`  | `TOGETHER_API_KEY`                                            |
| Ollama            | `llama3.3`                                 | _(无需 key；通过 `LLM_BASE_URL` override URL)_                |
| OpenAI-compatible | _(必须指定 model)_                         | _(可选；设置 `LLM_BASE_URL` + `LLM_MODEL`)_                   |

OpenRouter 和 OpenAI-compatible provider 没有默认 model —— 必须显式设置 `LLM_MODEL`。OpenAI-compatible provider 还要求设置 `LLM_BASE_URL`，它通过 OpenAI client 路由到任何兼容 OpenAI chat-completions API 的 server（LM Studio、vLLM、各类 gateway）。

## 解决 merge conflict

当你的 repo 处于 merge 中时运行 `aic resolve`。它会读取每个冲突文件，给出一份不含 marker 的解决方案，向你展示 diff，并逐文件询问 `apply?`。批准你信任的部分，其余保持原样。当没有未 merge 的内容残留时，它会替你执行该 merge 的 `--continue`。

你也可以在 conflict 的 repo 里直接运行普通的 `aic` —— 它会察觉并提议移交给 resolve，同时一个 commit guard 会阻止任何仍带有 conflict marker 的 commit。

**v1 限制：** `aic resolve` 只处理 conflict 的 **merge** 状态 —— 若检测到 rebase 或 `am` 进行中则会被拒绝。Binary、超大体积、以及 delete/modify 类型的 conflict 会被跳过并给出原因，由你手动解决。Finalize 是 all-or-nothing：只要还有未 merge 的路径，`--continue` 就会被阻塞，hand-off 也会明确告诉你还剩什么。

## Contributing

详见 [CONTRIBUTING.zh-CN.md](./CONTRIBUTING.zh-CN.md) — 分支规则、提交风格、评审预期。(English: [CONTRIBUTING.md](./CONTRIBUTING.md))

## License

[MIT](https://github.com/CaicoLeung/aic/blob/main/LICENSE)
