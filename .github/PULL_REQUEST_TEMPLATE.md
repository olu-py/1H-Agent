# Pull Request

## 变更类型

- [ ] 文档
- [ ] 核心（protium-core）
- [ ] TUI adapter
- [ ] 工具/存储/安全
- [ ] CI/发布

## 协议影响检查清单（改动 `protocol.rs` / `bridge.rs` / `conformance.rs` 必填）

按事件链逐环确认，指南见 `.agents/guides/ui-contract.md`：

- [ ] 加法演进：新变体/字段旧 UI 可忽略；未改名、未重排、未复用旧 tag
- [ ] `conformance.rs`：穷尽变体目录已补（编译强制）、至少一个新场景、契约测试通过
- [ ] JSON 夹具无漂移并已提交（`cargo test --lib --features test-util conformance`）
- [ ] TUI 映射已更新（projection 穷尽 match），`cargo test --lib conformance` 回放通过
- [ ] ts-rs bindings 已再生成（`cargo test` 导出测试，无漂移）
- [ ] `.agents/guides/ui-contract.md` 顺序保证与文档已同步

## 验证

- [ ] `cargo fmt --all -- --check`
- [ ] 最小目标测试已运行（核心改动 `-p protium-core`；根包改动在仓库根）
- [ ] 跨模块改动：`cargo clippy --all-targets --all-features --locked -- -D warnings` + `cargo test --workspace --all-features --locked`
- [ ] 文档改动：`bash scripts/check-agent-docs.sh` + `git diff --check`
