//! The project-agnostic loader runtime: the shared HTTP client (`net`) + the
//! FHS-entry / component-mount / pool-discovery / splash / cache+lock substrate
//! (`runtime`, re-exported at the crate root). The consuming launcher re-exports
//! these (`pub(crate) use loader_core::*`) so its lifecycle state machine drives them.
pub mod net;
mod runtime;
pub use runtime::*;
/// The crash-safe self-updater: manifest diff + download (`update`) and the
/// power-loss-safe apply onto the drive (`apply`).
pub mod apply;
pub mod update;
