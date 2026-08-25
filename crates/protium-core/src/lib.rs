//! 1H-Agent core: the UI-independent application state machine, model layer,
//! tools, storage, permissions, and the shared v2 UI protocol.
//!
//! This crate must never depend on Axum, Tauri, ratatui, React, or any platform
//! WebView. Every interface (Web, TUI, Desktop) drives the same
//! [`service::AppService`] / [`service::AppHandle`] entry point and consumes
//! the [`protocol`] DTOs over the [`bridge::EventBridge`].

/// Shared conformance scenarios and stream invariants for UI adapters.
///
/// Enabled only via the non-default `test-util` feature so the shipped
/// library and binaries never carry fixture code; adapters enable it in
/// dev-dependencies and replay the same corpus the core asserts against.
#[cfg(feature = "test-util")]
pub mod conformance;

pub mod agent;
pub mod app;
pub mod bridge;
pub mod commands;
pub mod config;
pub mod input;
pub mod model;
pub mod prompt;
pub mod protocol;
pub mod provider;
pub mod secrets;
pub mod security;
pub mod service;
pub mod session;
pub mod settings;
pub mod storage;
pub mod tools;
