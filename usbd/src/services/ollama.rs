//! Ollama, run from the mounted component (no nix).
//!
//! This is a **thin override** of mac-mgmt-agent's own `services::ollama::Ollama`
//! (composition, since Rust has no inheritance): the upstream struct is wrapped
//! and its read-only surface — health, tunnels, inventory, sample, security — is
//! delegated, so usbd tracks upstream for free. Only the stick-specific methods
//! are overridden: install/setup/upgrade are no-ops (the binary lives on the
//! mounted dmg/squashfs, never installed), and `spawn_spec` runs the mounted
//! `<resources>/ollama serve` with the USB env (models dir, flavour, NixOS LD).
//!
//! The effective port is resolved once in `new()` and baked into the upstream
//! struct's config, so every delegated method reports the *bound* port, not the
//! merely-preferred one.

use std::future::Future;
use std::pin::Pin;
use std::path::PathBuf;

use anyhow::Result;
use mac_mgmt_common::{InventoryEntry, OllamaConfig, SecurityFinding};

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};
use mac_mgmt_agent::services::ollama::Ollama;

pub struct UsbOllamaService {
    /// Upstream service we delegate the read-only surface to. Its config is
    /// patched to the resolved host/port so its base_url is always correct.
    inner: Ollama,
    host: String,
    /// Effective (resolved) port — owned here so spawn/health/tunnel agree.
    port: u16,
    bin: PathBuf,
    models_dir: PathBuf,
    flavour: String,
    child_ld: Option<String>,
}

impl UsbOllamaService {
    pub fn new(cfg: &OllamaConfig, res: &Resources) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.port);
        // Hand the upstream struct a config pinned to the *bound* port so its
        // delegated tunnels/inventory/sample/health all hit the right place.
        let mut inner_cfg = cfg.clone();
        inner_cfg.port = port;
        Self {
            inner: Ollama::new(inner_cfg),
            host,
            port,
            bin: res.ollama_bin.clone(),
            models_dir: res.models_dir.clone(),
            flavour: cfg.flavour.clone(),
            child_ld: res.child_ld_library_path.clone(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl ManagedService for UsbOllamaService {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn binary_name(&self) -> &str {
        self.inner.binary_name()
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Managed
    }

    // Mounted, never installed/upgraded.
    fn ensure_installed(&self) -> Result<()> {
        Ok(())
    }
    fn ensure_setup(&self) -> Result<()> {
        Ok(())
    }
    fn repair(&self) -> Result<()> {
        Ok(())
    }
    fn check_and_upgrade(&self) -> Result<bool> {
        Ok(false)
    }

    fn spawn_spec(&self) -> SpawnSpec {
        use std::collections::HashMap;
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("OLLAMA_HOST".into(), format!("{}:{}", self.host, self.port));
        env.insert(
            "OLLAMA_MODELS".into(),
            self.models_dir.to_string_lossy().into_owned(),
        );
        env.entry("OLLAMA_KEEP_ALIVE".into())
            .or_insert_with(|| "5m".into());
        if !self.flavour.is_empty() {
            env.insert("PLANAI_OLLAMA_FLAVOUR".into(), self.flavour.clone());
        }
        if let Some(extra) = &self.child_ld {
            env.insert("LD_LIBRARY_PATH".into(), extra.clone());
        }
        SpawnSpec {
            program: self.bin.to_string_lossy().into_owned(),
            args: vec!["serve".into()],
            env,
        }
    }

    fn check_health(&self) -> Result<bool> {
        // Cheap blocking liveness with no runtime dependency: open the TCP port.
        // (Upstream's HTTP probe needs a tokio worker; the sync path may not be
        // on one.) The async probe below delegates to upstream's richer check.
        use std::net::{TcpStream, ToSocketAddrs};
        let addr = format!("{}:{}", self.host, self.port);
        let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            return Ok(false);
        };
        Ok(TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(2)).is_ok())
    }

    fn check_health_async(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        self.inner.check_health_async()
    }

    fn expose_tunnels(&self) -> Vec<TunnelDef> {
        self.inner.expose_tunnels()
    }

    fn service_inventory(&self) -> Pin<Box<dyn Future<Output = Vec<InventoryEntry>> + Send + '_>> {
        self.inner.service_inventory()
    }

    fn service_sample(&self) -> Pin<Box<dyn Future<Output = Vec<InventoryEntry>> + Send + '_>> {
        self.inner.service_sample()
    }

    fn service_security(&self) -> Pin<Box<dyn Future<Output = Vec<SecurityFinding>> + Send + '_>> {
        self.inner.service_security()
    }
}
