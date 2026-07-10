//! Hermes messaging gateway, run from the mounted hermes component (no nix).
//!
//! The sibling of [`super::hermes_dashboard`]: where the dashboard is the web
//! UI, the gateway is hermes' API server + messaging surface (`hermes gateway
//! run`). On the stick it's spawned as `<hermes python> -m hermes_cli.main
//! gateway run --replace` and shares HERMES_HOME with the dashboard (same
//! config.yaml / sessions). The gateway takes no host/port flags — it binds
//! from `$HERMES_HOME/.env` (API_SERVER_HOST / API_SERVER_PORT), so `ensure_setup`
//! writes those (to the resolved port) plus an API_SERVER_KEY (generated once,
//! preserved after) which gates the HTTP API.
//!
//! Upstream's [`Hermes`] service assumes the nix `hermes` binary on PATH and a
//! `~/.hermes` home, so almost all of it is overridden here; we wrap it only to
//! keep the same shape as the other services and delegate its `service_inventory`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use mac_mgmt_common::{HermesConfig, HermesGatewayConfig, InventoryEntry};
use std::future::Future;
use std::pin::Pin;

use crate::usb_config::UsbHermesConfig;

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};
use mac_mgmt_agent::services::hermes::Hermes;

pub struct UsbHermesGatewayService {
    host: String,
    /// Effective (resolved) port — written to .env so the gateway binds it.
    port: u16,
    python: PathBuf,
    /// HERMES_HOME — shared with the dashboard (config.yaml, sessions, .env).
    home_dir: PathBuf,
    child_ld: Option<String>,
    /// Upstream service, config-pinned to the resolved host/port. Wrapped for
    /// shape-consistency with the other services; we delegate only its
    /// read-only `service_inventory`.
    inner: Hermes,
}

impl UsbHermesGatewayService {
    pub fn new(cfg: &UsbHermesConfig, res: &Resources) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.gateway_port);
        let home_dir = if cfg.data_dir.is_empty() {
            res.data_dir.join("hermes")
        } else {
            PathBuf::from(&cfg.data_dir)
        };
        let inner = Hermes::new(HermesConfig {
            gateway: Some(HermesGatewayConfig {
                host: host.clone(),
                port,
            }),
            ..Default::default()
        });
        Self {
            host,
            port,
            python: res.hermes_python.clone(),
            home_dir,
            child_ld: res.child_ld_library_path.clone(),
            inner,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn env_path(&self) -> PathBuf {
        self.home_dir.join(".env")
    }
}

/// Merge `updates` into a dotenv-style file, preserving any other keys/lines and
/// only ever appending or replacing the given keys. Creates the file if absent.
fn merge_env_file(path: &std::path::Path, updates: &[(&str, String)]) {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(String::from).collect();
    for (key, val) in updates {
        let prefix = format!("{key}=");
        if let Some(line) = lines.iter_mut().find(|l| l.trim_start().starts_with(&prefix)) {
            *line = format!("{key}={val}");
        } else {
            lines.push(format!("{key}={val}"));
        }
    }
    let mut body = lines.join("\n");
    body.push('\n');
    // The .env carries API_SERVER_KEY (gates the gateway HTTP API) — owner-only.
    super::write_private(path, body.as_bytes());
}

/// Read a key's value from a dotenv-style file (first match wins).
fn read_env_key(path: &std::path::Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{key}=");
    content
        .lines()
        .map(str::trim_start)
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl ManagedService for UsbHermesGatewayService {
    fn name(&self) -> &str {
        // NB: upstream names this service "hermes"; on the stick "hermes-gateway"
        // keeps it distinct from the dashboard ("hermes-dashboard") and webui.
        "hermes-gateway"
    }

    fn binary_name(&self) -> &str {
        "hermes"
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Managed
    }

    fn ensure_installed(&self) -> Result<()> {
        Ok(())
    }

    fn ensure_setup(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home_dir).ok();
        // The gateway binds from $HERMES_HOME/.env. Always pin host/port to the
        // resolved values (the port can change between runs on a collision);
        // generate API_SERVER_KEY once and preserve it (it gates the HTTP API).
        let env_path = self.env_path();
        let key = read_env_key(&env_path, "API_SERVER_KEY")
            .unwrap_or_else(|| hex::encode(rand::random::<[u8; 16]>()));
        merge_env_file(
            &env_path,
            &[
                ("API_SERVER_HOST", self.host.clone()),
                ("API_SERVER_PORT", self.port.to_string()),
                ("API_SERVER_KEY", key),
            ],
        );
        Ok(())
    }

    fn repair(&self) -> Result<()> {
        Ok(())
    }
    fn check_and_upgrade(&self) -> Result<bool> {
        Ok(false)
    }

    fn spawn_spec(&self) -> SpawnSpec {
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert(
            "HERMES_HOME".into(),
            self.home_dir.to_string_lossy().into_owned(),
        );
        if let Some(extra) = &self.child_ld {
            env.insert("LD_LIBRARY_PATH".into(), extra.clone());
        }
        // `gateway run` takes no host/port flags — it reads them from .env
        // (written in ensure_setup). --replace evicts any stale instance.
        SpawnSpec {
            program: self.python.to_string_lossy().into_owned(),
            args: vec![
                "-m".into(),
                "hermes_cli.main".into(),
                "gateway".into(),
                "run".into(),
                "--replace".into(),
            ],
            env,
        }
    }

    fn check_health(&self) -> Result<bool> {
        // The HTTP API is bearer-gated; a TCP probe is the cheap liveness check.
        use std::net::{TcpStream, ToSocketAddrs};
        let addr = format!("{}:{}", self.host, self.port);
        let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            return Ok(false);
        };
        Ok(TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(2)).is_ok())
    }

    fn expose_tunnels(&self) -> Vec<TunnelDef> {
        vec![TunnelDef {
            name: "hermes-gateway".into(),
            host: self.host.clone(),
            tcp_port: self.port,
        }]
    }

    fn service_inventory(&self) -> Pin<Box<dyn Future<Output = Vec<InventoryEntry>> + Send + '_>> {
        self.inner.service_inventory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_setup_writes_env_and_preserves_key() {
        let base = std::env::temp_dir().join(format!("hermes-gw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let svc = UsbHermesGatewayService {
            host: "127.0.0.1".into(),
            port: 8642,
            python: PathBuf::from("/x/python3"),
            home_dir: base.clone(),
            child_ld: None,
            inner: Hermes::new(HermesConfig::default()),
        };
        svc.ensure_setup().unwrap();
        let env = std::fs::read_to_string(base.join(".env")).unwrap();
        assert!(env.contains("API_SERVER_PORT=8642"));
        assert!(env.contains("API_SERVER_HOST=127.0.0.1"));
        let key1 = read_env_key(&base.join(".env"), "API_SERVER_KEY").unwrap();
        assert!(!key1.is_empty());
        // Second run on a different port must keep the key but update the port.
        let svc2 = UsbHermesGatewayService { port: 9999, ..svc_clone(&base) };
        svc2.ensure_setup().unwrap();
        let env2 = std::fs::read_to_string(base.join(".env")).unwrap();
        assert!(env2.contains("API_SERVER_PORT=9999"));
        assert_eq!(read_env_key(&base.join(".env"), "API_SERVER_KEY").as_deref(), Some(key1.as_str()));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn svc_clone(base: &std::path::Path) -> UsbHermesGatewayService {
        UsbHermesGatewayService {
            host: "127.0.0.1".into(),
            port: 8642,
            python: PathBuf::from("/x/python3"),
            home_dir: base.to_path_buf(),
            child_ld: None,
            inner: Hermes::new(HermesConfig::default()),
        }
    }

    #[test]
    fn spawn_runs_gateway_from_env() {
        let svc = svc_clone(std::path::Path::new("/data/hermes"));
        let spec = svc.spawn_spec();
        assert!(spec.program.ends_with("python3"));
        assert_eq!(
            spec.args,
            ["-m", "hermes_cli.main", "gateway", "run", "--replace"]
        );
        assert_eq!(spec.env.get("HERMES_HOME").map(String::as_str), Some("/data/hermes"));
    }
}
