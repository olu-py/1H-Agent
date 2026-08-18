# 发布维护指南

仅在修改版本、CI、安装包、release notes、tag 或 GitHub Release 时读取本文件。

## 先看哪些文件

- `Cargo.toml` / `Cargo.lock`：`protium-agent` 版本和锁定依赖。
- `.github/workflows/ci.yml`：三平台日常验证。
- `.github/workflows/release.yml`：tag 校验、测试、构建、安装包和发布资产。
- `.github/release-notes/`：按 tag 命名的正式发布说明。
- `scripts/package-linux.sh` / `scripts/package-windows.ps1`：本地打包辅助，不是 GitHub Release 的权威流程。

## 必须保持的不变量

- `Cargo.toml` 与 `Cargo.lock` 中项目版本一致，release tag 必须是同版本的新 `vX.Y.Z` annotated tag。
- 先提交并推送 `main`，核对远端 SHA 后再建 tag；不得移动或复用已成功发布的 tag。
- 正式发布由 `.github/workflows/release.yml` 完成：版本校验 -> 三平台验证 -> 四目标归档和安装包 -> checksums -> GitHub Release。
- release notes 固定为 `.github/release-notes/vX.Y.Z.md`；tag 推送前必须存在，否则 publish job 会失败。
- 归档保留 README、LICENSE、第三方声明和示例配置；Release 包含平台归档、DEB、MSI、`SHA256SUMS.txt` 与 `THIRD_PARTY_NOTICES.md`。
- 不假定本机有 `gh`，不在命令、文件、日志或回复中暴露凭据。GitHub Actions 使用最小必要权限和仓库 token。

## 常见故障顺序

- push 长时间无结果：macOS `osxkeychain` 可能等待解锁；不要把进程退出或“Pushing”文字当成功，用远端 SHA 或 GitHub API 核验。
- tag workflow 未启动：检查 tag 是否真的在远端、名称是否匹配 `v*`、tag 指向是否包含 notes 和版本提交。
- version job 失败：比较去掉 `v` 的 tag、Cargo metadata 和 lockfile package 版本。
- publish job 失败：先检查四个 build/installers artifact，再检查 notes 路径、checksums 命令和 Release 权限。
- 某平台失败：保持平台专用 shell 和路径语义；不要用只在本机可用的工具替代工作流已有命令。

## 最小验证命令

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo build --release --locked
git diff --check
```

推送前再核对 release notes 文件名、版本、远端 SHA 和 tag 指向；发布后检查 Release 非 draft/prerelease 且资产齐全。
