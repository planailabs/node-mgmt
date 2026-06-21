//! Hermes agent dashboard, run from the mounted hermes component (no nix).
//! Spawns `<hermes python> -m hermes_cli.main dashboard` directly — the FastAPI
//! web dashboard (it serves HERMES_WEB_DIST; the messaging gateway is a separate
//! `hermes gateway` mode we don't run by default). First run seeds a minimal
//! `config.yaml` pointing hermes at the LOCAL ollama (provider `custom`,
//! `base_url http://<host>:<port>/v1`, no API key) so it never needs the network
//! or a setup wizard. HERMES_HOME lives in the writable data dir; the component's
//! venv is read-only (mounted), so lazy dep installs go to the overlay venv there
//! (see the plan-ai-usb hermes/patches).

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Result;
use crate::usb_config::UsbHermesConfig;
use mac_mgmt_common::{HermesDashboardConfig, InventoryEntry, OllamaConfig};

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};
use mac_mgmt_agent::services::hermes_dashboard::HermesDashboard;

pub struct UsbHermesDashboardService {
    host: String,
    /// Effective (resolved) port.
    port: u16,
    python: PathBuf,
    web_dist: PathBuf,
    /// HERMES_HOME (config.yaml, sessions, overlay venv) — writable.
    home_dir: PathBuf,
    /// Local models dir, used to auto-pick a default model for config.yaml.
    models_dir: PathBuf,
    /// The LOCAL ollama OpenAI-compatible endpoint (effective port).
    ollama_base_url: String,
    default_model: Option<String>,
    child_ld: Option<String>,
    /// Upstream service (config-pinned to the resolved host/port) — we delegate
    /// its name + read-only tunnels/inventory; the spawn/setup/health below are
    /// overridden for the mounted-python run.
    inner: HermesDashboard,
}

impl UsbHermesDashboardService {
    pub fn new(
        cfg: &UsbHermesConfig,
        ollama_cfg: &OllamaConfig,
        res: &Resources,
        ollama_effective_port: Option<u16>,
    ) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.port);
        let home_dir = if cfg.data_dir.is_empty() {
            res.data_dir.join("hermes")
        } else {
            PathBuf::from(&cfg.data_dir)
        };
        let ollama_port = ollama_effective_port.unwrap_or(ollama_cfg.port);
        let inner = HermesDashboard::new(HermesDashboardConfig {
            enabled: true,
            host: host.clone(),
            port, // bound port, so delegated tunnels/inventory report it
        });
        Self {
            host,
            port,
            python: res.hermes_python.clone(),
            web_dist: res.hermes_web_dist.clone(),
            home_dir,
            models_dir: res.models_dir.clone(),
            ollama_base_url: format!("http://{}:{}", ollama_cfg.host, ollama_port),
            default_model: cfg.default_model.clone(),
            child_ld: res.child_ld_library_path.clone(),
            inner,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// The model seeded into a fresh config.yaml: the configured one, else the
    /// first model in the local ollama store (models/manifests/<registry>/
    /// <namespace>/<name>/<tag> → "name:tag").
    fn pick_model(&self) -> Option<String> {
        if let Some(m) = &self.default_model {
            return Some(m.clone());
        }
        first_local_model(&self.models_dir)
    }
}

/// Scan an ollama models dir for the first pulled model, as "name:tag".
fn first_local_model(models_dir: &Path) -> Option<String> {
    let manifests = models_dir.join("manifests");
    let mut registries: Vec<_> = std::fs::read_dir(&manifests).ok()?.flatten().collect();
    registries.sort_by_key(|e| e.file_name());
    for reg in registries {
        let mut namespaces: Vec<_> = std::fs::read_dir(reg.path()).ok()?.flatten().collect();
        namespaces.sort_by_key(|e| e.file_name());
        for ns in namespaces {
            let mut names: Vec<_> = std::fs::read_dir(ns.path()).ok()?.flatten().collect();
            names.sort_by_key(|e| e.file_name());
            for name in names {
                let mut tags: Vec<_> = std::fs::read_dir(name.path()).ok()?.flatten().collect();
                tags.sort_by_key(|e| e.file_name());
                if let Some(tag) = tags.first() {
                    return Some(format!(
                        "{}:{}",
                        name.file_name().to_string_lossy(),
                        tag.file_name().to_string_lossy()
                    ));
                }
            }
        }
    }
    None
}

impl ManagedService for UsbHermesDashboardService {
    fn name(&self) -> &str {
        self.inner.name() // "hermes-dashboard"
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Managed
    }

    fn ensure_installed(&self) -> Result<()> {
        Ok(())
    }

    fn ensure_setup(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home_dir).ok();
        // Seed a minimal config.yaml ONCE: provider `custom` + the local ollama
        // base_url means hermes is "configured" (no first-run wizard, no key)
        // and fully offline. Never overwrite — the user owns this file after.
        let cfg = self.home_dir.join("config.yaml");
        if !cfg.exists() {
            let model_line = self
                .pick_model()
                .map(|m| format!("  default: \"{m}\"\n"))
                .unwrap_or_default();
            let body = format!(
                "# seeded by the plan.ai usb daemon (first run) — yours to edit\n\
                 model:\n  provider: custom\n  base_url: \"{}/v1\"\n{model_line}",
                self.ollama_base_url,
            );
            std::fs::write(&cfg, body).ok();
        }
        Ok(())
    }

    fn repair(&self) -> Result<()> {
        Ok(())
    }
    fn check_and_upgrade(&self) -> Result<bool> {
        Ok(false)
    }

    fn spawn_spec(&self) -> SpawnSpec {
        let s = |p: &PathBuf| p.to_string_lossy().into_owned();
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("HERMES_HOME".into(), s(&self.home_dir));
        // The vite-built dashboard SPA — also suppresses the npm build path.
        env.insert("HERMES_WEB_DIST".into(), s(&self.web_dist));
        if let Some(extra) = &self.child_ld {
            env.insert("LD_LIBRARY_PATH".into(), extra.clone());
        }

        SpawnSpec {
            program: self.python.to_string_lossy().into_owned(),
            args: vec![
                "-m".into(),
                "hermes_cli.main".into(),
                "dashboard".into(),
                "--host".into(),
                self.host.clone(),
                "--port".into(),
                self.port.to_string(),
                "--no-open".into(),
                "--skip-build".into(),
            ],
            env,
        }
    }

    fn check_health(&self) -> Result<bool> {
        use std::net::{TcpStream, ToSocketAddrs};
        let addr = format!("{}:{}", self.host, self.port);
        let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            return Ok(false);
        };
        Ok(TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(2)).is_ok())
    }

    fn check_health_async(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let url = format!("{}/api/status", self.base_url());
            let client = reqwest::Client::new();
            match tokio::time::timeout(std::time::Duration::from_secs(5), client.get(&url).send()).await {
                Ok(Ok(resp)) => Ok(resp.status().is_success()),
                _ => Ok(false),
            }
        })
    }

    fn expose_tunnels(&self) -> Vec<TunnelDef> {
        self.inner.expose_tunnels()
    }

    fn service_inventory(&self) -> Pin<Box<dyn Future<Output = Vec<InventoryEntry>> + Send + '_>> {
        self.inner.service_inventory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_local_model_finds_name_tag() {
        let base = std::env::temp_dir().join(format!("hermes-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let lib = base.join("manifests/registry.ollama.ai/library");
        std::fs::create_dir_all(lib.join("smollm2/1.7b")).unwrap();
        assert_eq!(first_local_model(&base).as_deref(), Some("smollm2:1.7b"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn seeds_config_yaml_once() {
        let base = std::env::temp_dir().join(format!("hermes-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let svc = UsbHermesDashboardService {
            host: "127.0.0.1".into(),
            port: 9119,
            python: PathBuf::from("/x/python3"),
            web_dist: PathBuf::from("/x/web_dist"),
            home_dir: base.clone(),
            models_dir: base.join("no-models"),
            ollama_base_url: "http://127.0.0.1:11434".into(),
            default_model: Some("smollm2:1.7b".into()),
            child_ld: None,
            inner: HermesDashboard::new(HermesDashboardConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port: 9119,
            }),
        };
        svc.ensure_setup().unwrap();
        let cfg = std::fs::read_to_string(base.join("config.yaml")).unwrap();
        assert!(cfg.contains("provider: custom"));
        assert!(cfg.contains("base_url: \"http://127.0.0.1:11434/v1\""));
        assert!(cfg.contains("default: \"smollm2:1.7b\""));
        // never overwritten
        std::fs::write(base.join("config.yaml"), "user: edit\n").unwrap();
        svc.ensure_setup().unwrap();
        assert_eq!(std::fs::read_to_string(base.join("config.yaml")).unwrap(), "user: edit\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn spawn_spec_runs_dashboard_with_home_and_dist() {
        let svc = UsbHermesDashboardService {
            host: "127.0.0.1".into(),
            port: 9119,
            python: PathBuf::from("/mnt/res/hermes/python/bin/python3"),
            web_dist: PathBuf::from("/mnt/res/hermes/share/web_dist"),
            home_dir: PathBuf::from("/data/hermes"),
            models_dir: PathBuf::from("/data/models"),
            ollama_base_url: "http://127.0.0.1:11434".into(),
            default_model: None,
            child_ld: None,
            inner: HermesDashboard::new(HermesDashboardConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port: 9119,
            }),
        };
        let spec = svc.spawn_spec();
        assert!(spec.program.ends_with("python3"));
        assert_eq!(spec.args[..3], ["-m".to_string(), "hermes_cli.main".to_string(), "dashboard".to_string()]);
        assert!(spec.args.contains(&"--no-open".to_string()));
        assert!(spec.args.contains(&"--skip-build".to_string()));
        assert_eq!(spec.env.get("HERMES_HOME").map(String::as_str), Some("/data/hermes"));
        assert!(spec.env.contains_key("HERMES_WEB_DIST"));
    }
}
