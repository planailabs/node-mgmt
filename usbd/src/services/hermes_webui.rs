//! Hermes web UI (the `hermes-webui` component — three-panel sessions/chat/
//! workspace), run from its mounted component with the HERMES component's
//! portable python (it has every runtime dep: pyyaml + the agent modules the
//! UI imports). No venv of its own: HERMES_WEBUI_PYTHON / HERMES_WEBUI_AGENT_DIR
//! point bootstrap.py at the hermes tree, and HERMES_HOME is shared with the
//! dashboard service so both see the same config.yaml/sessions.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Result;
use crate::usb_config::UsbHermesConfig;
use mac_mgmt_common::{HermesWebuiConfig, InventoryEntry};

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};
use mac_mgmt_agent::services::hermes_webui::HermesWebui;

pub struct UsbHermesWebuiService {
    host: String,
    /// Effective (resolved) port.
    port: u16,
    /// The hermes component's python (also HERMES_WEBUI_PYTHON).
    python: PathBuf,
    /// The mounted hermes-webui component (bootstrap.py, api/, static/).
    webui_dir: PathBuf,
    /// HERMES_HOME — shared with the dashboard service (config.yaml, sessions).
    home_dir: PathBuf,
    child_ld: Option<String>,
    /// Upstream service, config-pinned to the resolved host/port. We delegate
    /// its read-only `/version` inventory probe (the rest of its surface is
    /// nix/config-dir based and doesn't apply to the mounted run).
    inner: HermesWebui,
}

impl UsbHermesWebuiService {
    pub fn new(cfg: &UsbHermesConfig, res: &Resources) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.webui_port);
        let home_dir = if cfg.data_dir.is_empty() {
            res.data_dir.join("hermes")
        } else {
            PathBuf::from(&cfg.data_dir)
        };
        let inner = HermesWebui::new(HermesWebuiConfig {
            enabled: true,
            host: host.clone(),
            port, // bound port, so the delegated /version probe hits the right place
            ..Default::default()
        });
        Self {
            host,
            port,
            python: res.hermes_python.clone(),
            webui_dir: res.hermes_webui_dir.clone(),
            home_dir,
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

    /// The hermes component's site-packages (HERMES_WEBUI_AGENT_DIR): the agent
    /// modules the UI imports at load. `<python root>/Lib/site-packages` on
    /// windows, `<python root>/lib/python3.X/site-packages` elsewhere.
    fn agent_site_packages(&self) -> Option<PathBuf> {
        let py_root = python_root(&self.python)?;
        let win = py_root.join("Lib").join("site-packages");
        if win.is_dir() {
            return Some(win);
        }
        let lib = py_root.join("lib");
        let mut vers: Vec<_> = std::fs::read_dir(&lib)
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("python"))
            .collect();
        vers.sort_by_key(|e| e.file_name());
        vers.first().map(|e| e.path().join("site-packages"))
    }
}

/// `<hermes>/python` from the interpreter path (`…/python/bin/python3` on
/// unix, `…/python/python.exe` on windows).
fn python_root(python: &Path) -> Option<PathBuf> {
    let parent = python.parent()?;
    if parent.file_name().is_some_and(|n| n == "bin") {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(parent.to_path_buf())
    }
}

impl ManagedService for UsbHermesWebuiService {
    fn name(&self) -> &str {
        "hermes-webui"
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Managed
    }

    fn ensure_installed(&self) -> Result<()> {
        Ok(())
    }

    fn ensure_setup(&self) -> Result<()> {
        // config.yaml seeding belongs to the hermes dashboard service (same
        // HERMES_HOME); just make sure the home exists for a webui-only run.
        std::fs::create_dir_all(&self.home_dir).ok();
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
        env.insert("HERMES_WEBUI_PYTHON".into(), s(&self.python));
        if let Some(sp) = self.agent_site_packages() {
            env.insert("HERMES_WEBUI_AGENT_DIR".into(), s(&sp));
        }
        env.insert("HERMES_WEBUI_HOST".into(), self.host.clone());
        env.insert("HERMES_WEBUI_PORT".into(), self.port.to_string());
        if let Some(extra) = &self.child_ld {
            env.insert("LD_LIBRARY_PATH".into(), extra.clone());
        }

        SpawnSpec {
            program: self.python.to_string_lossy().into_owned(),
            args: vec![
                self.webui_dir.join("bootstrap.py").to_string_lossy().into_owned(),
                self.port.to_string(),
                "--host".into(),
                self.host.clone(),
                "--no-browser".into(),
                "--skip-agent-install".into(),
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
            let url = self.base_url();
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
        // Upstream probes the running WebUI's `/version` endpoint — works the
        // same whether the binary was nix-installed or mounted.
        self.inner.service_inventory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_root_unix_and_windows_layouts() {
        assert_eq!(
            python_root(Path::new("/mnt/hermes/python/bin/python3")),
            Some(PathBuf::from("/mnt/hermes/python"))
        );
        assert_eq!(
            python_root(Path::new("/mnt/hermes/python/python.exe")),
            Some(PathBuf::from("/mnt/hermes/python"))
        );
    }
}
