//! 1H-Agent TUI: the Ratatui/Crossterm terminal frontend.
//!
//! The TUI is a thin adapter over the UI-independent protium_core state
//! machine. It owns only the terminal shell, layout/theme/markdown rendering,
//! mouse and menu interactions, and a display projection
//! (projection::TuiSessionProjection) that mirrors the active session by
//! consuming v2 protocol::Event envelopes from the core EventBridge. All
//! mutation (submit, commands, approvals, cancel, session switching, provider
//! settings) is serialized through service::AppHandle; the TUI never touches
//! the core's internal runtime, storage, tool registry or approval senders.

pub mod app;
pub mod clipboard;
pub mod home;
pub mod input;
pub mod output;
pub mod projection;
pub mod ui;
pub mod ui_layout;
pub mod ui_theme;
pub mod ui_view_model;

pub use protium_core::{
    agent, commands, config, model, provider, secrets, security, session, settings, storage, tools,
};
