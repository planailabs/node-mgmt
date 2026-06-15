//! The plan.ai control-plane contract, defined ONCE so the launcher (real), the
//! mock-server (dev preview), and the wasm SPA can't drift.
//!
//! - [`types`] — serde-only DTOs + state enums. The wire shapes. Always available;
//!   the wasm SPA depends on this crate with `default-features = false` for them.
//! - `server` (feature `server`, default-on) — the [`ControlApi`] trait + [`router`]
//!   the two backends implement.
//!
//! See `standards/control-api.md`.

mod types;
pub use types::*;

// The generic updater DTOs live in the loader framework; re-export them so the SPA,
// mock, and launcher keep importing `plan_ai_control_api::{UpdateState, UpdateStatus}`.
pub use loader_manifest::{UpdateState, UpdateStatus};

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::*;
