//! All launcher HTTP goes through one shared reqwest client: the llmfit proxy
//! (serve.rs), service health checks (control.rs) and update downloads (update.rs).
//! rustls TLS with the RING provider — installed as the process default here,
//! since reqwest's `*-no-provider` feature deliberately doesn't pick one (we avoid
//! aws-lc-rs so the launcher still cross-compiles + stays static-musl).

use std::sync::OnceLock;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// The shared, connection-pooled client (cheap to clone). Installs the ring
/// crypto provider as the process default on first use.
pub fn client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            // Idempotent; ignore the Err if something already installed a provider.
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .user_agent(concat!("plan-ai-launcher/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}
