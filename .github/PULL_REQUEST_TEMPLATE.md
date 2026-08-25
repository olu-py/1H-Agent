# Pull Request

## 变更类型

- [ ] 文档
- [ ] core Git 依赖更新 / TUI 适配
- [ ] TUI adapter
- [ ] 工具/存储/安全
- [ ] CI/发布

## 协议影响检查清单（更新 core Git 依赖时必填）

按事件链逐环确认，指南见 `.agents/guides/ui-contract.md`：

- [ ] 加法演进：新变体/字段旧 UI 可忽略；未改名、未重排、未复用旧 tag
- [ ] 上游 core commit 已完成协议、conformance、JSON 夹具和 bindings 验证
- [ ] `Cargo.lock` 只定向更新到预期 core commit
- [ ] TUI 映射已更新（projection 穷尽 match），`cargo test --lib conformance` 回放通过
- [ ] TUI 对新增事件/字段的 projection 与展示已适配
- [ ] `.agents/guides/ui-contract.md` 顺序保证与上游 core 文档一致

## 验证

- [ ] `cargo fmt --all -- --check`
- [ ] 最小目标测试已运行（core 更新至少运行 `cargo test --lib conformance`）
- [ ] 跨模块改动：`cargo clippy --all-targets --all-features --locked -- -D warnings` + `cargo test --workspace --all-features --locked`
- [ ] 文档改动：`bash scripts/check-agent-docs.sh` + `git diff --check`
