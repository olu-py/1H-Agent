//! Event bridge: the global bounded replay ring for UI events.
//!
//! Every routed agent event is converted to a [`protocol::Event`], wrapped in
//! an [`Envelope`] with a process-global monotonic cursor, and both appended to
//! a bounded in-memory ring and broadcast live. Consumers subscribe from a
//! known cursor: events after it are replayed from the ring (stored as
//! `Arc<Envelope>` to avoid copying large strings), then the live broadcast
//! takes over. When a requested cursor has been evicted from the ring, the
//! consumer is told to `resync` — refetch the snapshot and message page rather
//! than guess the missing state.
//!
//! Both limits (event count and total bytes) are configurable and clamped; a
//! slow consumer that lags the broadcast channel is detected and told to
//! resync rather than dropping events silently.

use std::{
    collections::VecDeque,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::broadcast;

use crate::protocol::{Envelope, Event};

/// Result of a replay request from a cursor.
#[derive(Clone, Debug)]
pub enum ReplayResult {
    /// Buffered events strictly after the requested cursor, oldest first.
    Replay(Vec<Arc<Envelope>>),
    /// The requested cursor was evicted; the consumer must resync.
    ResyncRequired,
}

/// Default maximum number of events retained in the ring.
pub const DEFAULT_MAX_EVENTS: usize = 512;
/// Default maximum total bytes retained in the ring.
pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Bounds applied to the ring capacity on construction.
pub const MIN_MAX_EVENTS: usize = 16;
pub const MAX_MAX_EVENTS: usize = 4096;
pub const MIN_MAX_BYTES: usize = 1024 * 1024;
pub const MAX_MAX_BYTES: usize = 16 * 1024 * 1024;

struct Ring {
    events: VecDeque<Arc<Envelope>>,
    bytes: usize,
}

impl Ring {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            bytes: 0,
        }
    }
}

fn string_bytes(value: &str) -> usize {
    value.len()
}

fn tool_call_bytes(call: &crate::provider::ToolCall) -> usize {
    call.id.len() + call.name.len() + call.arguments.to_string().len() + 32
}

fn event_payload_bytes(event: &Event) -> usize {
    use crate::protocol::Event as E;
    match event {
        E::ReasoningDelta { delta } => string_bytes(delta),
        E::ProviderRetry {
            reason, delay_ms, ..
        } => string_bytes(reason) + 16 + (*delay_ms as usize / 16),
        E::ModelStreaming => 0,
        E::WebSearchStarted { query } => string_bytes(query),
        E::WebSearchResult {
            title,
            url,
            snippet,
        } => string_bytes(title) + string_bytes(url) + string_bytes(snippet),
        E::WebSearchCompleted { .. } => 8,
        E::Cancelled { reason } => string_bytes(reason),
        E::TextDelta { delta } => string_bytes(delta),
        E::Approval {
            call,
            reason,
            source_session_id,
            source_title,
            ..
        } => {
            tool_call_bytes(call)
                + string_bytes(reason)
                + source_session_id.as_ref().map_or(0, String::len)
                + source_title.as_ref().map_or(0, String::len)
        }
        E::ApprovalResolved { .. } => 24,
        E::ToolStarted { call } => tool_call_bytes(call),
        E::ToolFinished { call, result } => tool_call_bytes(call) + string_bytes(result),
        E::Usage { .. } => 24,
        E::Completed => 0,
        E::Failed { error } => string_bytes(error),
        E::SessionsChanged => 0,
        E::ChildSessionProgress {
            child_session_id,
            status,
            tool,
            ..
        } => {
            string_bytes(child_session_id)
                + string_bytes(status)
                + tool.as_ref().map_or(0, String::len)
        }
        E::LocalCommandFinished { command, result } => string_bytes(command) + string_bytes(result),
        E::CompactionStarted => 0,
        E::CompactionCompleted { .. } => 8,
        E::CompactionFailed { error } => string_bytes(error),
        E::TodoUpdated { tasks } => tasks
            .iter()
            .map(|task| {
                task.id.len() + task.title.len() + task.created_at.len() + task.updated_at.len()
            })
            .sum(),
        E::TranscriptInvalidated => 0,
        E::ResyncRequired => 0,
    }
}

fn envelope_bytes(envelope: &Envelope) -> usize {
    // A cheap upper bound: cursor + session_id + serialized payload length.
    envelope.session_id.len() + 16 + event_payload_bytes(&envelope.event)
}

/// Shared event fan-out with a global replay ring.
///
/// All methods are synchronous: `push` only touches an in-memory ring and a
/// broadcast channel, so the state-machine task never blocks on I/O.
#[derive(Clone)]
pub struct EventBridge {
    tx: broadcast::Sender<Arc<Envelope>>,
    ring: Arc<RwLock<Ring>>,
    next_cursor: Arc<AtomicU64>,
    max_events: usize,
    max_bytes: usize,
}

impl EventBridge {
    pub fn new(max_events: usize, max_bytes: usize) -> Self {
        let max_events = max_events.clamp(MIN_MAX_EVENTS, MAX_MAX_EVENTS);
        let max_bytes = max_bytes.clamp(MIN_MAX_BYTES, MAX_MAX_BYTES);
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            ring: Arc::new(RwLock::new(Ring::new())),
            next_cursor: Arc::new(AtomicU64::new(0)),
            max_events,
            max_bytes,
        }
    }

    /// Registers an event: assigns the next process-global cursor, appends it to
    /// the replay ring (evicting the oldest entry when a limit is exceeded), and
    /// broadcasts it live. Returns the envelope so callers can reuse it.
    pub fn push(&self, session_id: String, event: Event) -> Arc<Envelope> {
        let cursor = self.next_cursor.fetch_add(1, Ordering::SeqCst);
        let envelope = Arc::new(Envelope {
            cursor,
            session_id,
            event,
        });
        {
            let mut ring = self
                .ring
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            ring.bytes = ring.bytes.saturating_add(envelope_bytes(&envelope));
            ring.events.push_back(envelope.clone());
            while ring.events.len() > self.max_events || ring.bytes > self.max_bytes {
                let Some(oldest) = ring.events.pop_front() else {
                    break;
                };
                ring.bytes = ring.bytes.saturating_sub(envelope_bytes(&oldest));
            }
        }
        // Receivers that lag the channel fall back to the ring on reconnect.
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    /// The next cursor value to be assigned (i.e., the number of events pushed
    /// so far). Snapshot responses return this as `event_cursor` so a fresh
    /// consumer subscribes from exactly here.
    pub fn current_cursor(&self) -> u64 {
        self.next_cursor.load(Ordering::SeqCst)
    }

    /// Subscribes to live events. The consumer must also call
    /// [`Self::replay_after`] *before* subscribing (replay first, then live) so
    /// events pushed between the two calls are never missed or duplicated.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Envelope>> {
        self.tx.subscribe()
    }

    /// Returns the buffered events strictly after `after`, oldest first.
    /// Returns [`ReplayResult::ResyncRequired`] when `after` is older than the
    /// oldest retained event (or predates every retained event), meaning the
    /// consumer must refetch snapshot + message page.
    pub fn replay_after(&self, after: u64) -> ReplayResult {
        let ring = self
            .ring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(oldest) = ring.events.front() else {
            if after < self.current_cursor() {
                return ReplayResult::ResyncRequired;
            }
            return ReplayResult::Replay(Vec::new());
        };
        if after < oldest.cursor {
            return ReplayResult::ResyncRequired;
        }
        ReplayResult::Replay(
            ring.events
                .iter()
                .filter(|envelope| envelope.cursor > after)
                .cloned()
                .collect(),
        )
    }

    /// The configured ring capacity (clamped).
    pub fn max_events(&self) -> usize {
        self.max_events
    }

    /// The configured ring byte cap (clamped).
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Event;

    fn text(delta: &str) -> Event {
        Event::TextDelta {
            delta: delta.into(),
        }
    }

    #[test]
    fn cursors_are_global_and_monotonic() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        assert_eq!(bridge.current_cursor(), 0);
        for i in 0..10 {
            bridge.push("a".into(), text(&format!("{i}")));
            assert_eq!(bridge.current_cursor(), i + 1);
        }
        bridge.push("b".into(), text("b"));
        assert_eq!(bridge.current_cursor(), 11);
    }

    #[test]
    fn replay_after_returns_only_newer_events_in_order() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        for i in 0..10 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        match bridge.replay_after(3) {
            ReplayResult::Replay(events) => {
                assert_eq!(events.len(), 6);
                assert_eq!(events[0].cursor, 4);
                assert_eq!(events[5].cursor, 9);
            }
            ReplayResult::ResyncRequired => panic!("cursor 3 must still be buffered"),
        }
    }

    #[test]
    fn evicted_cursor_requires_resync() {
        let bridge = EventBridge::new(16, DEFAULT_MAX_BYTES);
        for i in 0..32 {
            bridge.push("a".into(), text(&format!("{i}")));
        }
        // The ring only keeps the last 16 (cursors 16..=31); 15 is evicted.
        assert!(matches!(
            bridge.replay_after(15),
            ReplayResult::ResyncRequired
        ));
        // The oldest retained cursor replays the tail without resync.
        match bridge.replay_after(16) {
            ReplayResult::Replay(events) => assert_eq!(events.len(), 15),
            ReplayResult::ResyncRequired => panic!("cursor 16 must still be buffered"),
        }
    }

    #[test]
    fn byte_cap_evicts_largest_payloads_first_in_fifo_order() {
        // Small byte cap forces eviction even with a large event budget.
        let bridge = EventBridge::new(256, 2 * 1024 * 1024);
        let big = "x".repeat(4 * 1024 * 1024);
        bridge.push("a".into(), text(&big));
        // The single event exceeds the byte cap, so the ring must evict it.
        assert!(matches!(
            bridge.replay_after(0),
            ReplayResult::ResyncRequired
        ));
    }

    #[test]
    fn empty_ring_replays_nothing_without_resync_when_up_to_date() {
        let bridge = EventBridge::new(16, DEFAULT_MAX_BYTES);
        match bridge.replay_after(0) {
            ReplayResult::Replay(events) => assert!(events.is_empty()),
            ReplayResult::ResyncRequired => panic!("nothing was pushed, nothing was evicted"),
        }
    }

    #[test]
    fn live_broadcast_carries_arc_envelopes() {
        let bridge = EventBridge::new(64, DEFAULT_MAX_BYTES);
        let mut receiver = bridge.subscribe();
        let pushed = bridge.push("s".into(), text("hi"));
        let received = receiver.try_recv().expect("live event");
        assert_eq!(received.cursor, pushed.cursor);
        assert_eq!(received.session_id, "s");
        assert!(Arc::ptr_eq(&received, &pushed));
    }
}
