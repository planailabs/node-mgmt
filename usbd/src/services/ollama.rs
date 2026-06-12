//! Ollama, run from the mounted component (no nix). Mirrors the daemon's
//! `services/ollama.rs` shape but spawns `<resources>/ollama serve` directly and
//! never installs or upgrades — the binary lives on the mounted dmg/squashfs.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::Result;
use mac_mgmt_common::{InventoryEntry, InventoryValueType, OllamaConfig};

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};

pub struct UsbOllamaService {
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
        Self {
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

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

impl ManagedService for UsbOllamaService {
    fn name(&self) -> &str {
        "ollama"
    }

    fn binary_name(&self) -> &str {
        "ollama"
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
        // Cheap blocking liveness: can we open the TCP port?
        use std::net::{TcpStream, ToSocketAddrs};
        let addr = format!("{}:{}", self.host, self.port);
        let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            return Ok(false);
        };
        Ok(TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(2)).is_ok())
    }

    fn check_health_async(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let url = format!("{}/api/version", self.base_url());
            let client = reqwest::Client::new();
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.get(&url).send(),
            )
            .await
            {
                Ok(Ok(resp)) => Ok(resp.status().is_success()),
                _ => Ok(false),
            }
        })
    }

    fn expose_tunnels(&self) -> Vec<TunnelDef> {
        vec![TunnelDef {
            name: "ollama".into(),
            host: self.host.clone(),
            tcp_port: self.port,
        }]
    }

    fn service_sample(&self) -> Pin<Box<dyn Future<Output = Vec<InventoryEntry>> + Send + '_>> {
        Box::pin(async move {
            // Loaded models from /api/ps (best-effort).
            let url = format!("{}/api/ps", self.base_url());
            let client = reqwest::Client::new();
            let Ok(Ok(resp)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), client.get(&url).send())
                    .await
            else {
                return Vec::new();
            };
            let Ok(json) = resp.json::<serde_json::Value>().await else {
                return Vec::new();
            };
            let loaded: Vec<String> = json
                .get("models")
                .and_then(|m| m.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            vec![InventoryEntry {
                id: "loaded_models".into(),
                name: "Loaded Models".into(),
                value: serde_json::json!(loaded),
                value_type: InventoryValueType::Json,
            }]
        })
    }
}
