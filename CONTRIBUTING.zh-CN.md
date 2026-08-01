# 参与 aic 开发

感谢你的贡献！🎉 本指南让评审循环更快、历史更干净。

> English version: [CONTRIBUTING.md](./CONTRIBUTING.md)

## 开始之前

- **先读 ADR** — 架构决策记录在 [`docs/adr/`](./docs/adr/)。如果你的改动触及架构，先看看有没有相关 ADR；如果你的改动本身就是值得记录的决策，请新增一条。
- **非平凡功能先开 issue 或先讨论** — 上游先对齐一下，省得来回返工。

## 本地环境

```bash
cargo build          # stable 工具链，由 rust-toolchain.toml 锁定
cargo test           # 单元测试内联在 src/ 中（#[cfg(test)] 模块）
```

## Git 历史规则

- **禁止 merge commit。** 保持分支线性 — 用 rebase 而不是把 main 合进来。merge commit 在合并时会被拒绝。
- **Conventional Commits** 提交信息（`feat:`、`fix:`、`refactor:`、`docs:`、`test:`、`chore:` ...）— aic 自己就生成这类信息，项目与自身工具保持一致。

## 提交 PR 之前

1. 运行 `cargo fmt --all` 并提交格式化结果
2. 运行 `cargo clippy --all-targets -- -D warnings` 并修复所有 warning — CI 对任何 warning 都会失败
3. 运行 `cargo test` — 为新行为添加测试（`src/` 中的内联 `#[cfg(test)]` 模块，或 `tests/` 下的 integration test）
4. 填写 [PR template](./.github/pull_request_template.md) — 做了什么、为什么、如何验证

## 评审预期

- CI 会在每个 PR 上强制执行 `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test` 和 `cargo deny`（安全公告 / license 检查）
- maintainer 会评审；你可能会被要求修改 — 这很正常，保持 diff 聚焦在被要求的内容上
- CI 绿且 review 通过后，改动会以 **squash** 方式合并
- 分支规则拒绝 merge commit，所以基于 rebase 的更新是预期行为 — 如果你的分支被重写过，那是为了保持历史线性，不是抹掉你的工作

## 发布

发布由 maintainer 通过 `scripts/release.sh` 处理（`prepare-release.sh` 负责 changelog）。你的 PR 不需要手动 bump 版本。
