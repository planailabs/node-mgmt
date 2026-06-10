//! plan.ai dashboard — Dioxus web/wasm SPA. Served + embedded by the rust
//! launcher (phase 3/4). Talks to the launcher's same-origin control API
//! (/api/*) and reuses the plan-ai-design component library.

mod api;
mod app;
#[cfg(feature = "future")]
mod config;
mod dashboard;
mod models;

fn main() {
    dioxus::launch(app::App);
}
