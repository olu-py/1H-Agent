//! Generates TypeScript bindings for the v2 UI wire protocol.
//!
//! Writes one `.ts` file per exported type into `TS_RS_EXPORT_DIR` (default
//! `web/ts/` relative to the workspace root). `export_all` recurses into every
//! dependency annotated with `#[ts(export)]`, so the output is self-contained.
//! CI regenerates to a temporary directory and compares against the committed
//! output to prevent Rust/TS contract drift.

use protium_core::{
    model::{TodoStatus, TodoTask},
    protocol::{
        ApiError, ApiErrorKind, AppSnapshotV2, ApprovalDto, Envelope, Event, MessageDto,
        MessagePage, SessionStateDto, TodoDto,
    },
    provider::ToolCall,
};
use ts_rs::{Config, TS};

fn export<T: TS + 'static>(cfg: &Config, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    T::export_all(cfg)?;
    println!("exported {name}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // i64/u64 cursors and message ids are SQLite rowids: far below 2^53, so
    // represent them as JS `number` rather than `bigint` for ergonomics.
    let cfg = Config::from_env().with_large_int("number");

    export::<Envelope>(&cfg, "Envelope")?;
    export::<Event>(&cfg, "Event")?;
    export::<AppSnapshotV2>(&cfg, "AppSnapshotV2")?;
    export::<SessionStateDto>(&cfg, "SessionStateDto")?;
    export::<ApprovalDto>(&cfg, "ApprovalDto")?;
    export::<TodoDto>(&cfg, "TodoDto")?;
    export::<TodoTask>(&cfg, "TodoTask")?;
    export::<TodoStatus>(&cfg, "TodoStatus")?;
    export::<MessageDto>(&cfg, "MessageDto")?;
    export::<MessagePage>(&cfg, "MessagePage")?;
    export::<ApiError>(&cfg, "ApiError")?;
    export::<ApiErrorKind>(&cfg, "ApiErrorKind")?;
    export::<ToolCall>(&cfg, "ToolCall")?;

    println!("generated type bindings in {}", cfg.out_dir().display());
    Ok(())
}
