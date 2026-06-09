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

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::*;
