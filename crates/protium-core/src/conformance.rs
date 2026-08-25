//! Shared conformance corpus for every UI adapter of the v2 protocol.
//!
//! This module is the single source of truth for protocol-level replay
//! fixtures. It exists only under the non-default `test-util` feature:
//! adapters (TUI today, WebUI/Desktop later) enable it in dev-dependencies
//! and replay the exact same [`Envelope`] streams the core asserts against,
//! so "core adds an event, every adapter must adapt" becomes mechanical -
//! a new [`Event`] variant breaks the exhaustive [`variant_name`] match at
//! compile time, and the corpus coverage test then fails until a scenario
//! carries the new variant. With that, each adapter's replay test goes red
//! until it handles the new event.
//!
//! The scenarios are also exported as JSON fixtures under
//! `crates/protium-core/conformance/` (drift-checked by test, mirroring the
//! ts-rs bindings export pattern) so a WebUI repository can run the same
//! corpus without a Rust toolchain.
//!
//! Ordering rules encoded by [`check_stream_invariants`] come from the
//! UI-contract guide, not from this module's imagination: cursor strictly
//! increasing; within one model round `ReasoningDelta* -> ReasoningCompleted`
//! (exactly once, only when the round produced reasoning) `-> TextDelta* ->
//! ToolCallStreaming*` (monotonic `received_bytes`) `-> Approval/ToolStarted`;
//! a terminal event ends its round and occurs at most once per round. Events
//! observed before any `ModelStreaming` (a replay slice joining mid-round)
//! are tolerated: the round boundary is unknown in a slice.

use serde::Serialize;

use crate::model::{TodoStatus, TodoTask};
use crate::protocol::{ContextBudgetDto, Envelope, Event};
use crate::provider::ToolCall;

/// The fixed session id used by every scenario envelope (except
/// [`Event::ResyncRequired`], which the contract defines as session-less).
pub const SESSION_ID: &str = "conformance";

/// The end-state a consumer should be in after replaying a scenario. Adapters
/// assert against this instead of re-deriving expectations per event, so one
/// scenario drives every adapter's postcondition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Expectation {
    /// The agent is mid-round (thinking, streaming text/tool args, or running
    /// a tool); the consumer shows an active state.
    Active,
    /// The round finished successfully; the consumer is back to an idle,
    /// ready state.
    Completed,
    /// The round failed; the consumer shows the error terminal state.
    Failed,
    /// The round was cancelled; the consumer is back to idle.
    Cancelled,
    /// An approval is outstanding; the consumer must be showing it.
    ApprovalPending,
    /// The consumer's event cursor was evicted; it must refetch snapshot and
    /// message page instead of guessing state.
    Resync,
}

/// One named, deterministic [`Envelope`] stream plus the expected end-state.
#[derive(Clone, Debug, Serialize)]
pub struct Scenario {
    pub name: &'static str,
    pub description: &'static str,
    pub expectation: Expectation,
    pub envelopes: Vec<Envelope>,
}

/// Deterministic envelope builder: cursors count up from 1 and every envelope
/// targets [`SESSION_ID`].
struct Stream {
    envelopes: Vec<Envelope>,
}

impl Stream {
    fn new() -> Self {
        Self {
            envelopes: Vec::new(),
        }
    }

    fn push(&mut self, event: Event) {
        let cursor = self.envelopes.len() as u64 + 1;
        self.envelopes.push(Envelope {
            cursor,
            session_id: SESSION_ID.to_owned(),
            event,
        });
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    }
}

fn todo(id: &str, title: &str, status: TodoStatus) -> TodoTask {
    TodoTask {
        id: id.to_owned(),
        title: title.to_owned(),
        status,
        created_at: "2025-01-01T00:00:00Z".to_owned(),
        updated_at: "2025-01-01T00:00:00Z".to_owned(),
    }
}

fn budget(used_tokens: u64) -> ContextBudgetDto {
    ContextBudgetDto {
        context_window_tokens: Some(128_000),
        used_tokens,
        output_reserve_tokens: 8_192,
        safe_input_tokens: Some(128_000 - 8_192 - used_tokens),
        estimated: false,
    }
}

fn approval_event(approval_id: &str, call: ToolCall) -> Event {
    Event::Approval {
        approval_id: approval_id.to_owned(),
        call,
        reason: "写入工作区文件".to_owned(),
        source_session_id: None,
        source_title: None,
    }
}

/// The full replay corpus. Every [`Event`] variant must appear in at least
/// one scenario (enforced by test); add a scenario whenever the protocol
/// grows.
pub fn scenarios() -> Vec<Scenario> {
    let mut all = Vec::new();

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningDelta {
            delta: "正在分析项目结构".to_owned(),
        });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta {
            delta: "结构确认完毕。".to_owned(),
        });
        stream.push(Event::TextDelta {
            delta: "可以开始修改。".to_owned(),
        });
        stream.push(Event::Usage {
            input_tokens: 1_200,
            output_tokens: 80,
            total_tokens: 1_280,
        });
        stream.push(Event::Completed);
        all.push(Scenario {
            name: "reasoning_then_answer",
            description: "round with reasoning barrier, body deltas, usage, completion",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::TextDelta {
            delta: "直接回答，没有思考。".to_owned(),
        });
        stream.push(Event::Completed);
        all.push(Scenario {
            name: "answer_without_reasoning",
            description: "round without reasoning never emits ReasoningCompleted",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let call = tool_call(
            "c1",
            "file_write",
            serde_json::json!({"path": "a.txt", "content": "after"}),
        );
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningDelta {
            delta: "需要写入文件".to_owned(),
        });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta {
            delta: "先更新文件内容。".to_owned(),
        });
        stream.push(Event::ToolCallStreaming {
            name: Some("file_write".to_owned()),
            received_bytes: 900,
        });
        stream.push(Event::ToolCallStreaming {
            name: Some("file_write".to_owned()),
            received_bytes: 1_800,
        });
        stream.push(approval_event("ap1", call.clone()));
        stream.push(Event::ApprovalResolved {
            approval_id: "ap1".to_owned(),
            approved: true,
        });
        stream.push(Event::ToolStarted { call: call.clone() });
        stream.push(Event::ToolFinished {
            call,
            result: "已写入 a.txt".to_owned(),
        });
        stream.push(Event::ModelStreaming);
        stream.push(Event::TextDelta {
            delta: "文件已更新。".to_owned(),
        });
        stream.push(Event::Completed);
        all.push(Scenario {
            name: "tool_round_with_approval",
            description: "reasoning, body, streamed tool args, approval, tool run, follow-up round",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::TextDelta {
            delta: "正在生成大文件写入……".to_owned(),
        });
        stream.push(Event::ToolCallStreaming {
            name: None,
            received_bytes: 512,
        });
        stream.push(Event::ToolCallStreaming {
            name: Some("file_write".to_owned()),
            received_bytes: 1_100,
        });
        stream.push(Event::ToolCallStreaming {
            name: Some("file_write".to_owned()),
            received_bytes: 2_300,
        });
        all.push(Scenario {
            name: "tool_call_streaming_progress",
            description: "monotonic tool-arg streaming progress, stream cut mid-generation",
            expectation: Expectation::Active,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ProviderRetry {
            attempt: 1,
            reason: "rate limited".to_owned(),
            delay_ms: 1_000,
        });
        stream.push(Event::ReasoningDelta {
            delta: "重试后继续".to_owned(),
        });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta {
            delta: "成功恢复。".to_owned(),
        });
        stream.push(Event::Completed);
        all.push(Scenario {
            name: "provider_retry_then_success",
            description: "provider retry surfaced mid-round, round still completes",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ProviderRetry {
            attempt: 1,
            reason: "rate limited".to_owned(),
            delay_ms: 1_000,
        });
        stream.push(Event::ProviderRetry {
            attempt: 2,
            reason: "rate limited".to_owned(),
            delay_ms: 2_000,
        });
        stream.push(Event::Failed {
            error: "provider exhausted retries".to_owned(),
        });
        all.push(Scenario {
            name: "provider_retry_then_failure",
            description: "retries exhausted, round fails terminally",
            expectation: Expectation::Failed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::WebSearchStarted {
            query: "rust broadcast channel".to_owned(),
        });
        stream.push(Event::WebSearchResult {
            title: "tokio docs".to_owned(),
            url: "https://docs.rs".to_owned(),
            snippet: "broadcast channel semantics".to_owned(),
        });
        stream.push(Event::WebSearchCompleted { count: 1 });
        stream.push(Event::TextDelta {
            delta: "搜索结果确认……".to_owned(),
        });
        stream.push(Event::Completed);
        all.push(Scenario {
            name: "web_search_round",
            description: "native web search interleaved with a completing round",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningDelta {
            delta: "用户按下了取消".to_owned(),
        });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta {
            delta: "部分正文".to_owned(),
        });
        stream.push(Event::Cancelled {
            reason: "user pressed Esc".to_owned(),
        });
        all.push(Scenario {
            name: "cancelled_round",
            description: "user cancels mid-round; terminal Cancelled state",
            expectation: Expectation::Cancelled,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::TextDelta {
            delta: "流式中途出错".to_owned(),
        });
        stream.push(Event::Failed {
            error: "connection reset".to_owned(),
        });
        all.push(Scenario {
            name: "failed_round",
            description: "stream fails mid-round; terminal Failed state",
            expectation: Expectation::Failed,
            envelopes: stream.envelopes,
        });
    }

    {
        let call = tool_call("c9", "shell", serde_json::json!({"command": "cargo test"}));
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningDelta {
            delta: "需要跑测试".to_owned(),
        });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta {
            delta: "执行测试命令。".to_owned(),
        });
        stream.push(Event::ToolCallStreaming {
            name: Some("shell".to_owned()),
            received_bytes: 64,
        });
        stream.push(approval_event("ap9", call));
        all.push(Scenario {
            name: "approval_pending",
            description: "stream ends with an outstanding approval; consumer must show it",
            expectation: Expectation::ApprovalPending,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::TextDelta {
            delta: "回答完成".to_owned(),
        });
        stream.push(Event::Usage {
            input_tokens: 2_000,
            output_tokens: 100,
            total_tokens: 2_100,
        });
        stream.push(Event::Completed);
        stream.push(Event::ContextUpdated {
            budget: budget(2_100),
        });
        all.push(Scenario {
            name: "usage_and_context_update",
            description: "usage then engine-pushed ContextUpdated after the terminal",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::CompactionStarted);
        stream.push(Event::CompactionCompleted { hidden: 12 });
        stream.push(Event::ContextUpdated {
            budget: budget(3_000),
        });
        stream.push(Event::TranscriptInvalidated);
        all.push(Scenario {
            name: "compaction_round",
            description: "compaction command flow outside an agent round",
            expectation: Expectation::Active,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::CompactionStarted);
        stream.push(Event::CompactionFailed {
            error: "summary provider error".to_owned(),
        });
        all.push(Scenario {
            name: "compaction_failed",
            description: "failed compaction falls back to safe trimming",
            expectation: Expectation::Active,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::TodoUpdated {
            tasks: vec![
                todo("t1", "定位契约测试入口", TodoStatus::Done),
                todo("t2", "补充场景夹具", TodoStatus::InProgress),
                todo("t3", "同步消费端映射", TodoStatus::Pending),
            ],
        });
        stream.push(Event::LocalCommandFinished {
            command: "cargo test -p protium-core".to_owned(),
            result: "ok. 12 passed".to_owned(),
        });
        all.push(Scenario {
            name: "todo_and_local_command",
            description: "todo list update and a finished local (!) command",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::SessionsChanged);
        all.push(Scenario {
            name: "sessions_changed",
            description: "session list changed; consumer refreshes the list",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningDelta {
            delta: "主 Agent 思考中".to_owned(),
        });
        stream.push(Event::ChildSessionProgress {
            child_session_id: "child-1".to_owned(),
            status: "running".to_owned(),
            turn: 2,
            max_turns: 10,
            tool: Some("file_read".to_owned()),
        });
        stream.push(Event::ChildSessionProgress {
            child_session_id: "child-1".to_owned(),
            status: "completed".to_owned(),
            turn: 3,
            max_turns: 10,
            tool: None,
        });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta {
            delta: "子任务完成，汇总结果。".to_owned(),
        });
        all.push(Scenario {
            name: "child_progress_interleaved",
            description: "child-agent progress interleaves mid-round without breaking phases",
            expectation: Expectation::Active,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::TranscriptInvalidated);
        all.push(Scenario {
            name: "transcript_invalidated",
            description: "history-modifying command; consumer refetches message pages",
            expectation: Expectation::Completed,
            envelopes: stream.envelopes,
        });
    }

    {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        all.push(Scenario {
            name: "model_streaming_only",
            description: "round opened, stream cut before any delta",
            expectation: Expectation::Active,
            envelopes: stream.envelopes,
        });
    }

    // ResyncRequired is session-less per the contract: push it manually
    // instead of through the SESSION_ID builder.
    all.push(Scenario {
        name: "resync_required",
        description: "cursor evicted; consumer must refetch snapshot and message page",
        expectation: Expectation::Resync,
        envelopes: vec![Envelope {
            cursor: 1,
            session_id: String::new(),
            event: Event::ResyncRequired,
        }],
    });

    all
}

/// Exhaustive variant catalog. Adding an [`Event`] variant breaks this match
/// at compile time, which forces the new variant into the catalog, which the
/// corpus coverage test then forces into a scenario.
pub fn variant_name(event: &Event) -> &'static str {
    match event {
        Event::ReasoningDelta { .. } => "reasoning_delta",
        Event::ReasoningCompleted => "reasoning_completed",
        Event::ProviderRetry { .. } => "provider_retry",
        Event::ModelStreaming => "model_streaming",
        Event::WebSearchStarted { .. } => "web_search_started",
        Event::WebSearchResult { .. } => "web_search_result",
        Event::WebSearchCompleted { .. } => "web_search_completed",
        Event::Cancelled { .. } => "cancelled",
        Event::TextDelta { .. } => "text_delta",
        Event::ToolCallStreaming { .. } => "tool_call_streaming",
        Event::Approval { .. } => "approval",
        Event::ApprovalResolved { .. } => "approval_resolved",
        Event::ToolStarted { .. } => "tool_started",
        Event::ToolFinished { .. } => "tool_finished",
        Event::Usage { .. } => "usage",
        Event::Completed => "completed",
        Event::Failed { .. } => "failed",
        Event::SessionsChanged => "sessions_changed",
        Event::ChildSessionProgress { .. } => "child_session_progress",
        Event::LocalCommandFinished { .. } => "local_command_finished",
        Event::CompactionStarted => "compaction_started",
        Event::CompactionCompleted { .. } => "compaction_completed",
        Event::CompactionFailed { .. } => "compaction_failed",
        Event::TodoUpdated { .. } => "todo_updated",
        Event::TranscriptInvalidated => "transcript_invalidated",
        Event::ContextUpdated { .. } => "context_updated",
        Event::ResyncRequired => "resync_required",
    }
}

/// Phase of a model round, as observed by the invariant checker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoundPhase {
    /// No `ModelStreaming` seen yet in this stream (a replay slice may join
    /// mid-round, so ordering before the first observed round is unchecked).
    Unopened,
    Reasoning,
    Body,
    ToolStreaming,
    ToolExecution,
    Ended,
}

/// Validates the documented stream invariants over an [`Envelope`] sequence.
/// Returns every violation found (empty = the stream is conformant).
///
/// Only rules documented in the UI-contract guide are enforced:
/// - cursors strictly increase;
/// - within an observed round: `ReasoningDelta*` then `ReasoningCompleted`
///   (exactly once, only after a reasoning delta; the phase transition makes
///   a repeat a structural violation) then `TextDelta*` then
///   `ToolCallStreaming*` (non-decreasing `received_bytes`) then
///   `Approval`/`ToolStarted`;
/// - a terminal event ends the round and occurs at most once per round;
///   round-scoped events after it require a fresh `ModelStreaming`.
pub fn check_stream_invariants(envelopes: &[Envelope]) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();
    let mut last_cursor = 0u64;
    let mut phase = RoundPhase::Unopened;
    let mut round = 0usize;
    let mut reasoning_deltas = 0usize;
    let mut tool_stream_bytes: Option<u64> = None;

    for (index, envelope) in envelopes.iter().enumerate() {
        if envelope.cursor <= last_cursor {
            violations.push(format!(
                "envelope {index} ({}): cursor {} does not strictly increase (previous {})",
                variant_name(&envelope.event),
                envelope.cursor,
                last_cursor
            ));
        }
        last_cursor = envelope.cursor;

        match &envelope.event {
            Event::ModelStreaming => {
                round += 1;
                phase = RoundPhase::Reasoning;
                reasoning_deltas = 0;
                tool_stream_bytes = None;
            }
            Event::ReasoningDelta { .. } => {
                if phase == RoundPhase::Reasoning {
                    reasoning_deltas += 1;
                } else if phase != RoundPhase::Unopened {
                    violations.push(format!(
                        "envelope {index}: reasoning delta outside the reasoning phase of round {round}"
                    ));
                }
            }
            Event::ReasoningCompleted => {
                if phase == RoundPhase::Reasoning {
                    if reasoning_deltas == 0 {
                        violations.push(format!(
                            "envelope {index}: ReasoningCompleted without any reasoning delta in round {round}"
                        ));
                    }
                    phase = RoundPhase::Body;
                } else if phase != RoundPhase::Unopened {
                    violations.push(format!(
                        "envelope {index}: ReasoningCompleted outside the reasoning phase of round {round}"
                    ));
                }
            }
            Event::TextDelta { .. } => match phase {
                RoundPhase::Unopened | RoundPhase::Reasoning | RoundPhase::Body => {}
                RoundPhase::ToolStreaming | RoundPhase::ToolExecution | RoundPhase::Ended => {
                    violations.push(format!(
                        "envelope {index}: text delta after tool phase of round {round}"
                    ));
                }
            },
            Event::ToolCallStreaming { received_bytes, .. } => match phase {
                RoundPhase::Unopened => {}
                RoundPhase::Reasoning | RoundPhase::Body | RoundPhase::ToolStreaming => {
                    if let Some(previous) = tool_stream_bytes
                        && *received_bytes < previous
                    {
                        violations.push(format!(
                            "envelope {index}: tool call streaming bytes went backwards in round {round} ({previous} -> {received_bytes})"
                        ));
                    }
                    tool_stream_bytes = Some(*received_bytes);
                    phase = RoundPhase::ToolStreaming;
                }
                RoundPhase::ToolExecution | RoundPhase::Ended => {
                    violations.push(format!(
                        "envelope {index}: tool call streaming after tool execution began in round {round}"
                    ));
                }
            },
            Event::Approval { .. } | Event::ToolStarted { .. } | Event::ToolFinished { .. } => {
                match phase {
                    RoundPhase::Unopened
                    | RoundPhase::Reasoning
                    | RoundPhase::Body
                    | RoundPhase::ToolStreaming
                    | RoundPhase::ToolExecution => {
                        phase = RoundPhase::ToolExecution;
                    }
                    RoundPhase::Ended => violations.push(format!(
                        "envelope {index}: tool event after terminal of round {round}"
                    )),
                }
            }
            Event::Completed | Event::Failed { .. } | Event::Cancelled { .. } => {
                if phase == RoundPhase::Ended {
                    violations.push(format!(
                        "envelope {index}: second terminal event in round {round}"
                    ));
                } else if phase != RoundPhase::Unopened {
                    phase = RoundPhase::Ended;
                }
            }
            // Phase-neutral by contract: they may interleave anywhere.
            Event::ProviderRetry { .. }
            | Event::WebSearchStarted { .. }
            | Event::WebSearchResult { .. }
            | Event::WebSearchCompleted { .. }
            | Event::Usage { .. }
            | Event::SessionsChanged
            | Event::ChildSessionProgress { .. }
            | Event::LocalCommandFinished { .. }
            | Event::CompactionStarted
            | Event::CompactionCompleted { .. }
            | Event::CompactionFailed { .. }
            | Event::TodoUpdated { .. }
            | Event::TranscriptInvalidated
            | Event::ContextUpdated { .. }
            | Event::ApprovalResolved { .. }
            | Event::ResyncRequired => {}
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("conformance")
    }

    #[test]
    fn corpus_covers_every_catalogued_variant() {
        let catalog = [
            "reasoning_delta",
            "reasoning_completed",
            "provider_retry",
            "model_streaming",
            "web_search_started",
            "web_search_result",
            "web_search_completed",
            "cancelled",
            "text_delta",
            "tool_call_streaming",
            "approval",
            "approval_resolved",
            "tool_started",
            "tool_finished",
            "usage",
            "completed",
            "failed",
            "sessions_changed",
            "child_session_progress",
            "local_command_finished",
            "compaction_started",
            "compaction_completed",
            "compaction_failed",
            "todo_updated",
            "transcript_invalidated",
            "context_updated",
            "resync_required",
        ];
        let mut covered: Vec<&'static str> = Vec::new();
        for scenario in scenarios() {
            for envelope in &scenario.envelopes {
                let name = variant_name(&envelope.event);
                if !covered.contains(&name) {
                    covered.push(name);
                }
            }
        }
        for name in catalog {
            assert!(
                covered.contains(&name),
                "no conformance scenario covers Event variant {name}; add one"
            );
        }
        assert_eq!(
            covered.len(),
            catalog.len(),
            "catalog and corpus coverage drifted"
        );
    }

    #[test]
    fn scenarios_satisfy_stream_invariants() {
        for scenario in scenarios() {
            let result = check_stream_invariants(&scenario.envelopes);
            assert!(
                result.is_ok(),
                "scenario {} violates stream invariants: {:?}",
                scenario.name,
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn scenario_names_are_unique() {
        let all = scenarios();
        let mut names: Vec<&'static str> = all.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            count,
            "scenario names must be unique (they are file names too)"
        );
    }

    #[test]
    fn exports_scenario_fixtures_without_drift() {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).expect("create conformance fixture dir");
        let mut drifted: Vec<&'static str> = Vec::new();
        let mut expected_files: Vec<std::ffi::OsString> = Vec::new();
        for scenario in scenarios() {
            let path = dir.join(format!("{}.json", scenario.name));
            expected_files.push(path.file_name().unwrap().to_owned());
            let mut json = serde_json::to_string_pretty(&scenario).expect("serialize scenario");
            json.push('\n');
            match std::fs::read_to_string(&path) {
                Ok(existing) if existing == json => {}
                _ => {
                    std::fs::write(&path, json).expect("write conformance fixture");
                    drifted.push(scenario.name);
                }
            }
        }
        for entry in std::fs::read_dir(&dir).expect("read conformance fixture dir") {
            let entry = entry.expect("fixture dir entry");
            if entry.path().extension().is_some_and(|ext| ext == "json")
                && !expected_files.contains(&entry.file_name())
            {
                std::fs::remove_file(entry.path()).expect("remove stale fixture");
                drifted.push("stale-fixture-removed");
            }
        }
        assert!(
            drifted.is_empty(),
            "conformance fixtures drifted; commit the regenerated crates/protium-core/conformance/: {drifted:?}"
        );
    }

    #[test]
    fn invariant_checker_rejects_invalid_streams() {
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningDelta { delta: "d".into() });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta { delta: "t".into() });
        stream.push(Event::ToolStarted {
            call: tool_call("c1", "file_read", serde_json::json!({"path": "a"})),
        });
        stream.push(Event::TextDelta {
            delta: "late".into(),
        });
        stream.push(Event::Completed);
        stream.push(Event::Completed);
        let violations = check_stream_invariants(&stream.envelopes).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.contains("ReasoningCompleted outside the reasoning phase")),
            "expected repeated barrier violation, got {violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("text delta after tool phase")),
            "expected late text delta violation, got {violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.contains("second terminal")),
            "expected double terminal violation, got {violations:?}"
        );

        // Cursor regression.
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::TextDelta { delta: "a".into() });
        let mut envelopes = stream.envelopes.clone();
        envelopes.push(Envelope {
            cursor: 1,
            session_id: SESSION_ID.to_owned(),
            event: Event::Completed,
        });
        let violations = check_stream_invariants(&envelopes).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.contains("does not strictly increase")),
            "expected cursor violation, got {violations:?}"
        );

        // ReasoningCompleted without reasoning deltas, and streaming bytes
        // going backwards.
        let mut stream = Stream::new();
        stream.push(Event::ModelStreaming);
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::ToolCallStreaming {
            name: None,
            received_bytes: 2_000,
        });
        stream.push(Event::ToolCallStreaming {
            name: None,
            received_bytes: 1_000,
        });
        let violations = check_stream_invariants(&stream.envelopes).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.contains("without any reasoning delta")),
            "expected barrier-without-reasoning violation, got {violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("bytes went backwards")),
            "expected non-monotonic streaming violation, got {violations:?}"
        );
    }

    /// A replay slice that joins mid-round must not be flagged: the first
    /// observed events arrive without a preceding `ModelStreaming`.
    #[test]
    fn invariant_checker_tolerates_mid_round_slices() {
        let mut stream = Stream::new();
        stream.push(Event::ReasoningDelta { delta: "d".into() });
        stream.push(Event::ReasoningCompleted);
        stream.push(Event::TextDelta { delta: "t".into() });
        stream.push(Event::Completed);
        assert!(check_stream_invariants(&stream.envelopes).is_ok());
    }

    mod contract {
        //! Drives the real production chain - scripted provider -> AgentRunner
        //! forward closure -> `routed_to_event` protocol mapping - and locks
        //! the documented ordering guarantees on the mapped stream.

        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::sync::mpsc;

        use super::super::*;
        use crate::agent::{AgentEvent, AgentRunner};
        use crate::config::{ProviderPreset, RuntimeConfig};
        use crate::provider::{ConversationItem, ModelEvent, OpenAiClient, Role};
        use crate::security::Workspace;
        use crate::service::routed_to_event;
        use crate::storage::Storage;
        use crate::tools::ToolRegistry;

        fn write_call() -> crate::provider::ToolCall {
            crate::provider::ToolCall {
                id: "c1".to_owned(),
                name: "file_write".to_owned(),
                arguments: serde_json::json!({
                    "path": "a.txt",
                    "content": "after"
                }),
            }
        }

        #[tokio::test]
        async fn scripted_round_emits_documented_order() {
            let temp = TempDir::new().unwrap();
            std::fs::write(temp.path().join("a.txt"), "before").unwrap();
            let storage = Storage::open(&temp.path().join("agent.db")).unwrap();
            let session_id = storage.create_session(temp.path()).unwrap();
            storage
                .append_message(&session_id, Role::User, "edit file and explain")
                .unwrap();
            let tools = Arc::new(ToolRegistry::new(
                Workspace::new(temp.path()).unwrap(),
                RuntimeConfig::default(),
                false,
            ));
            let call = write_call();
            let large_argument_delta = "x".repeat(900);
            let provider = OpenAiClient::scripted(vec![
                vec![
                    ModelEvent::ReasoningDelta("需要修改文件".into()),
                    ModelEvent::TextDelta("我来更新内容。".into()),
                    ModelEvent::ToolCallDelta {
                        slot: "0".into(),
                        id: Some(call.id.clone()),
                        name: Some(call.name.clone()),
                        arguments_delta: large_argument_delta.clone(),
                    },
                    ModelEvent::ToolCallDelta {
                        slot: "0".into(),
                        id: Some(call.id.clone()),
                        name: Some(call.name.clone()),
                        arguments_delta: large_argument_delta.clone(),
                    },
                    ModelEvent::ToolCallDelta {
                        slot: "0".into(),
                        id: Some(call.id.clone()),
                        name: Some(call.name.clone()),
                        arguments_delta: large_argument_delta,
                    },
                    ModelEvent::ToolCallComplete(call.clone()),
                    ModelEvent::Done,
                ],
                vec![
                    ModelEvent::TextDelta("文件已更新。".into()),
                    ModelEvent::Done,
                ],
            ])
            .unwrap();
            let mut provider_config = ProviderPreset::Custom.defaults();
            provider_config.model = "fixture".into();
            let runner = AgentRunner::new(provider, provider_config, tools, storage, session_id);
            let (events, mut receiver) = mpsc::channel(64);
            let task = tokio::spawn(async move {
                runner
                    .run(
                        vec![ConversationItem::Message {
                            role: Role::User,
                            content: "edit file and explain".into(),
                        }],
                        events,
                    )
                    .await;
            });

            // Map every routed AgentEvent through the production mapping.
            // Approval is engine-level (fresh approval_id + oneshot), so it is
            // mirrored the way Engine::handle_routed does, including the
            // ApprovalResolved the engine emits once the decision resolves.
            let mut stream: Vec<Event> = Vec::new();
            while let Some(event) = receiver.recv().await {
                match event {
                    AgentEvent::Approval {
                        call,
                        reply,
                        reason,
                        source_session_id,
                        source_title,
                    } => {
                        stream.push(Event::Approval {
                            approval_id: "contract-ap1".to_owned(),
                            call,
                            reason,
                            source_session_id,
                            source_title,
                        });
                        let _ = reply.send(true);
                        stream.push(Event::ApprovalResolved {
                            approval_id: "contract-ap1".to_owned(),
                            approved: true,
                        });
                    }
                    other => {
                        if let Some(mapped) = routed_to_event(&other) {
                            stream.push(mapped);
                        }
                    }
                }
            }
            task.await.unwrap();

            // The mapped stream itself satisfies the documented invariants.
            let envelopes: Vec<Envelope> = stream
                .into_iter()
                .enumerate()
                .map(|(index, event)| Envelope {
                    cursor: index as u64 + 1,
                    session_id: SESSION_ID.to_owned(),
                    event,
                })
                .collect();
            if let Err(violations) = check_stream_invariants(&envelopes) {
                panic!("scripted round violated the documented order: {violations:?}");
            }

            let names: Vec<&'static str> = envelopes
                .iter()
                .map(|envelope| variant_name(&envelope.event))
                .collect();

            let position = |name: &'static str| names.iter().position(|n| *n == name);
            let last_of = |name: &'static str| names.iter().rposition(|n| *n == name);

            // Round 1 shape: ModelStreaming opens, the reasoning barrier sits
            // between the last ReasoningDelta and the first TextDelta, tool
            // args stream after the last TextDelta and before Approval, and
            // the round terminates only after the tool ran.
            assert_eq!(position("model_streaming"), Some(0));
            let reasoning = last_of("reasoning_delta").expect("reasoning delta");
            let barrier = position("reasoning_completed").expect("reasoning barrier");
            let first_text = position("text_delta").expect("text delta");
            assert!(reasoning < barrier && barrier < first_text);
            let last_text_round1 = first_text; // only one TextDelta in round 1
            let first_streaming = position("tool_call_streaming").expect("tool streaming");
            let approval = position("approval").expect("approval");
            let tool_started = position("tool_started").expect("tool started");
            let tool_finished = position("tool_finished").expect("tool finished");
            assert!(last_text_round1 < first_streaming && first_streaming < approval);
            assert!(approval < tool_started && tool_started < tool_finished);

            // Tool-arg streaming is monotonic.
            let mut bytes: Vec<u64> = Vec::new();
            for envelope in &envelopes {
                if let Event::ToolCallStreaming { received_bytes, .. } = &envelope.event {
                    bytes.push(*received_bytes);
                }
            }
            assert!(bytes.windows(2).all(|pair| pair[0] <= pair[1]));

            // A second round reopens with ModelStreaming and completes.
            let second_streaming = names
                .iter()
                .enumerate()
                .filter_map(|(index, name)| (*name == "model_streaming").then_some(index))
                .nth(1)
                .expect("second round ModelStreaming");
            let completed = position("completed").expect("completed terminal");
            assert!(tool_finished < second_streaming && second_streaming < completed);
            assert_eq!(names.last(), Some(&"completed"));
        }
    }
}
