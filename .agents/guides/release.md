# 发布维护指南

## 适用范围

版本、CI、release notes、tag、平台归档、安装包、GitHub Release，以及 workspace/core/adapter 的版本兼容验证。

## 入口

- `Cargo.toml`/`Cargo.lock`：版本；`.github/workflows/ci.yml`：三平台日常验证。
- `crates/protium-core/Cargo.toml`：核心版本；消费端依赖声明。
- `.github/workflows/release.yml`：正式发布；`.github/release-notes/`：按 tag 命名的说明。
- `scripts/package-*` 仅作本地辅助，不是 GitHub Release 的权威流程。

## 不变量

- Cargo manifest/lock 版本一致；tag 是同版本的新 annotated `vX.Y.Z`，不得移动或复用成功 tag。
- workspace/core/adapter 版本必须兼容：核心协议版本（`PROTOCOL_VERSION`）与各消费端兼容性在发布前验证；TUI/WebUI/Desktop adapter 分别验证，核心变更需覆盖所有消费端。
- 先提交并推送 main、核对远端 SHA，再建 tag；notes 文件必须是 `.github/release-notes/vX.Y.Z.md`。
- 权威流程：版本校验 -> 三平台验证 -> 四目标归档和安装包 -> checksums -> GitHub Release。
- 归档保留 README、LICENSE、第三方声明和示例配置；Release 包含归档、DEB、MSI、checksums 和 notices。
- 不假定本机有 `gh`；凭据不得进入命令、文件、日志或回复，Actions 使用最小权限 token。

## 诊断

| 症状 | 检查顺序 |
| --- | --- |
| push 无结果 | keychain 解锁 -> 远端 SHA/API；禁止把本地输出当成功 |
| workflow 未启动 | 远端 tag -> `v*` 匹配 -> tag 指向/notes/版本提交 |
| version 失败 | tag 去 `v` -> Cargo metadata -> lockfile version |
| publish 失败 | build/installers artifacts -> notes -> checksums -> 权限 |
| 单平台失败 | runner shell -> 路径语义 -> 平台专用依赖/命令 |
| 消费端协议不兼容 | `PROTOCOL_VERSION` -> 新旧事件/DTO 兼容 -> 各 adapter 构建与契约验证 |

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo build --release --locked
git diff --check
```

- 核心协议或 DTO 变更时，额外验证所有消费端 adapter（当前为 TUI）仍能构建并消费 v2 契约。
