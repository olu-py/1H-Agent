# 协议一致性夹具与多端适配规范化实施计划

> 状态：已归档。本文记录 conformance 初次实施，当前维护以 `.agents/guides/ui-contract.md` 和独立 [1H-Agent-core](https://github.com/olu-py/1H-Agent-core) 仓库为准。
> 文中的路径已按 core 抽离后的仓库结构更新，不再表示本仓库包含 core 源码。

## 背景与目标

维护模式：protium-core 功能演进，TUI/WebUI/Desktop 作为消费端适配。本计划把「各端必须适配」从人工纪律变成机器强制：

**成功标准**

1. 核心新增 `Event` 变体 -> conformance 模块的穷尽变体目录**编译失败** + 覆盖测试失败，直到补齐场景。
2. 每个场景同时被核心（顺序不变量断言）和 TUI（projection/facade 回放）消费——一份夹具测两端。
3. JSON 夹具产物带漂移检查（仿 bindings 导出模式），外部 WebUI 仓库可直接消费。
4. 文档与 PR 模板固化「事件链速查」，人或 AI 改协议都走同一张清单。

## 现状基线（计划批准时已核实）

- `protocol.rs` 是 DTO 单一权威；ts-rs 绑定由 `export_bindings_*` 测试再生成+漂移校验；TUI projection match（`src/projection.rs` `handle_event`）穷尽无兜底；`AppSnapshotV2.protocol_version` 已存在。
- `OpenAiClient::scripted` 为 `#[cfg(test)] pub(crate)`，agent.rs 已有完整 AgentRunner 测试 harness；`routed_to_event`（service.rs，AgentEvent->Event 映射）原为私有函数；Engine 层额外注入 Approval（fresh approval_id）与 ContextUpdated。
- TUI facade 的 `handle_envelope`（src/app.rs）可独立测试；事件泵 cursor 去重在 run 循环内。
- 文档约束：`check-agent-docs.sh` 限 AGENTS.md ≤85 行、各指南 ≤50 行；指南必须保留「适用范围/入口/不变量/诊断/验证」5 个固定小节。

## Phase 1（本次实施）

### 0. 计划落盘
新建 `docs/plans/protocol-conformance-plan.md`（本文件）。

### 1. conformance 模块（test-util feature）
- core 仓库 `Cargo.toml`：新增 `[features] test-util = []`（非默认，零新依赖）。
- core 仓库 `src/lib.rs`：`#[cfg(feature = "test-util")] pub mod conformance;`
- core 仓库新建 `src/conformance.rs`：
  - `pub struct Scenario { name, description, expectation: Expectation, envelopes: Vec<Envelope> }`；`pub enum Expectation { RoundStreaming, RoundCompleted, RoundFailed, RoundCancelled, ApprovalPending, Resync }`。
  - `pub fn scenarios()`：初始场景集覆盖全部 `Event` 变体（以覆盖测试为最终裁判）；cursor 从 1 递增、固定 session_id，保证确定性。
  - `pub fn check_stream_invariants(&[Envelope]) -> Result<(), Vec<String>>`：只实现 ui-contract.md 已文档化的规则——cursor 严格递增；同轮内 `ReasoningDelta* -> ReasoningCompleted`（仅一次、仅有思考时）`-> TextDelta* -> ToolCallStreaming*`（received_bytes 单调）`-> Approval/ToolStarted`；Approval 后终须 `ApprovalResolved` 或终态；终态每轮至多一次且居轮末。
  - `fn variant_name(&Event) -> &'static str`：穷尽 match 无通配——新增变体即编译失败，这是核心强制机制。
- core 仓库 `src/service.rs`：`routed_to_event` 改 `pub(crate)`（一词改动，对外仍不可见）。

### 2. 核心契约测试（scripted -> 真实映射 -> 不变量）
conformance.rs `#[cfg(test)]` 内：
- 变体覆盖测试：目录中每个变体至少出现在一个场景。
- `scripted_round_emits_documented_order`：复用 agent.rs harness（temp Storage + ToolRegistry + `OpenAiClient::scripted`）驱动 AgentRunner 跑含思考/正文/工具审批的完整轮次，逐条经 `routed_to_event` 映射，断言输出序列精确符合文档化顺序并通过 `check_stream_invariants`。
- JSON 导出漂移测试：各场景 `serde_json` 序列化后写 core 仓库 `conformance/<name>.json` 并比对（基于 `CARGO_MANIFEST_DIR`，仿 export_bindings 模式）。
- 不变量自检：全部场景通过 check；非法序列（乱序/重复终态）在测试内手构并断言报错。
- 不新增服务层注入机制（Phase 2）；Engine 附加事件由既有 service 测试覆盖。

### 3. TUI 回放同一批夹具
- 根 `Cargo.toml` dev-dependencies：Git `protium-core` 依赖启用 `test-util`（仅测试构建启用，release 二进制不含 conformance 代码）。
- `src/projection.rs` 测试：对每个 Scenario 新建 `TuiSessionProjection` 逐事件 `handle_event`，断言不 panic 且终态符合 expectation（如 RoundCompleted ⇒ 思考已提交、非 live；ApprovalPending ⇒ 审批挂起）。
- `src/app.rs`：
  - 微重构：从 run 循环抽出 `fn accept_envelope(app, &Envelope) -> bool`（cursor 去重 + handle_envelope + 推进），coalesce 逻辑留在循环内。
  - 测试 `corpus_replay_advances_cursor_and_dedups`：每场景从 event_cursor=0 回放，断言终 cursor==最后信封；重放同批（陈旧 cursor）状态不变。

### 4. 文档与流程固化
- `.agents/guides/ui-contract.md`：入口加 conformance.rs 与 JSON 产物；验证节加「新增事件 -> 场景 + 变体目录 + JSON 无漂移 + 契约测试 + TUI 回放」清单；注明 test-util 与 `--all-features` 语义（保持 ≤50 行）。
- `AGENTS.md`：任务路由表把协议夹具/契约测试指向独立 core 仓库与 UI Contract。
- `.agents/guides/tui.md`：验证节加夹具回放一行。
- 新建 `.github/PULL_REQUEST_TEMPLATE.md`：勾选式事件链速查（动 protocol.rs/bridge.rs ⇒ 绑定/JSON 漂移、场景覆盖、指南同步、各端映射）。

### 5. 验证（跨模块档位，一次全过）
```
cargo fmt --all -- --check
# 在 core 仓库
cargo test --lib --all-features --locked
# 在 TUI 仓库
cargo test --all-features --locked          # 根包 = TUI 回放测试
cargo clippy --all-targets --all-features --locked -- -D warnings
bash scripts/check-agent-docs.sh && git diff --check
```

## Phase 2（后续独立改动，本次不做）
1. 服务层端到端一致性：为 `rebuild_runner` 加 `#[cfg(test)]` scripted provider 注入（内部 App 结构 test-only 字段），AppService->Engine->bridge->`subscribe_from` 全链断言 Envelope 流与游标逐出 resync。
2. release.yml 打包 `bindings/` + `conformance/*.json` 附到 GitHub Release，供 WebUI 仓库版本化消费。
3. WebUI 接入约束（本仓库仅文档化）：exhaustive switch + assertNever、启动校验 `protocol_version`、共享 TS protocol-client 包（cursor 去重/resync/分页单实现）。

## 边界与不做
- 不改任何协议 wire 语义、不加 Deserialize、不动 bridge 容量语义。
- 不在本仓库实现 WebUI/TS 代码（AGENTS.md 范围约束）。
- 不新增第三方依赖。

## 风险与回退
- 指南行数超限 -> 精简措辞，跑 `check-agent-docs.sh` 验证。
- 不变量检查器误报（如 ChildSessionProgress 与主轮交错）-> 检查器只覆盖已文档化规则；真实流违反即文档/代码不一致，按此修复（这正是计划目的）。
- feature 统一化副作用 -> dev-deps feature 仅测试构建生效；`--all-features` 下 conformance 模块须过 clippy。
