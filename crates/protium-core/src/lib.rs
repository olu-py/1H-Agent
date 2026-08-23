//! 1H-Agent core: the UI-independent application state machine, model layer,
//! tools, storage, permissions, and the shared v2 UI protocol.
//!
//! This crate must never depend on Axum, Tauri, ratatui, React, or any platform
//! WebView. Every interface (Web, TUI, Desktop) drives the same
//! [`service::AppService`] / [`service::AppHandle`] entry point and consumes
//! the [`protocol`] DTOs over the [`bridge::EventBridge`].

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
