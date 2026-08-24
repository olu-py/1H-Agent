//! Generates TypeScript bindings for the v2 UI wire protocol.
//!
//! Writes one `.ts` file per exported type into the directory given by
//! `TS_RS_EXPORT_DIR` (`Config::from_env`); point it at
//! `crates/protium-core/bindings` to regenerate the committed files.
//! `export_all` recurses into every dependency annotated with `#[ts(export)]`,
//! so the output is self-contained.
//!
//! Note: the committed bindings are maintained and verified by the
//! `#[ts(export)]`-generated `export_bindings_*` unit tests during
//! `cargo test`, not by CI or by this binary. This binary configures
//! `with_large_int("number")` while the export tests use ts-rs's default
//! `bigint`, so the two outputs differ on large ints — treat the `cargo test`
//! output as authoritative for the committed files.

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
