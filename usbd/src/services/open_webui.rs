//! Open-WebUI, run from the mounted runtime (no nix). Spawns
//! `<runtime python> -m uvicorn open_webui.main:app` directly. The env mirrors
//! the launcher's offline-kiosk contract (`launcher/src/config.rs::webui_env`):
//! no auth by default, no runtime network fetches (HF/NLTK assets prebundled),
//! persistent secret keys under `DATA_DIR`.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::Result;
use mac_mgmt_common::{OllamaConfig, OpenWebuiConfig};

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};

pub struct UsbOpenWebuiService {
    host: String,
    /// Effective (resolved) port.
    port: u16,
    python: PathBuf,
    frontend: Option<PathBuf>,
    ow_assets: PathBuf,
    data_dir: PathBuf,
    auth: bool,
    secret_key: Option<String>,
    /// `OLLAMA_BASE_URL` for the chat backend (effective ollama port).
    ollama_base_url: String,
    child_ld: Option<String>,
}

impl UsbOpenWebuiService {
    pub fn new(
        cfg: &OpenWebuiConfig,
        ollama_cfg: &OllamaConfig,
        res: &Resources,
        ollama_effective_port: Option<u16>,
    ) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.port);
        let data_dir = if cfg.data_dir.is_empty() {
            res.data_dir.clone()
        } else {
            PathBuf::from(&cfg.data_dir)
        };
        let ollama_port = ollama_effective_port.unwrap_or(ollama_cfg.port);
        Self {
            host,
            port,
            python: res.webui_python.clone(),
            frontend: res.ow_frontend.clone(),
            ow_assets: res.ow_assets.clone(),
            data_dir,
            auth: cfg.auth,
            secret_key: cfg.secret_key.as_ref().map(|s| s.expose().to_string()),
            ollama_base_url: format!("http://{}:{}", ollama_cfg.host, ollama_port),
            child_ld: res.child_ld_library_path.clone(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Get-or-create a persistent hex secret under `DATA_DIR` so Open-WebUI's
    /// encrypted fields stay decryptable across runs (matches the launcher).
    fn persistent_secret(&self, name: &str) -> String {
        let f = self.data_dir.join(format!(".{name}"));
        if let Ok(s) = std::fs::read_to_string(&f) {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
        use rand::Rng;
        let bytes: [u8; 32] = rand::rng().random();
        let v = hex::encode(bytes);
        let _ = std::fs::create_dir_all(&self.data_dir);
        let _ = std::fs::write(&f, &v);
        v
    }
}

impl ManagedService for UsbOpenWebuiService {
    fn name(&self) -> &str {
        "open-webui"
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Managed
    }

    fn ensure_installed(&self) -> Result<()> {
        Ok(())
    }
    fn ensure_setup(&self) -> Result<()> {
        // sqlite needs DATA_DIR to exist before launch.
        std::fs::create_dir_all(&self.data_dir).ok();
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
        let assets = &self.ow_assets;
        let hf = s(&assets.join("hf"));

        let mut env: HashMap<String, String> = HashMap::new();
        if let Some(fe) = &self.frontend {
            env.insert("FRONTEND_BUILD_DIR".into(), s(fe));
        }
        env.insert("HOST".into(), self.host.clone());
        env.insert("PORT".into(), self.port.to_string());
        env.insert("OLLAMA_BASE_URL".into(), self.ollama_base_url.clone());
        env.insert(
            "WEBUI_AUTH".into(),
            if self.auth { "True".into() } else { "False".into() },
        );
        let secret = self
            .secret_key
            .clone()
            .unwrap_or_else(|| self.persistent_secret("secret-key"));
        env.insert("WEBUI_SECRET_KEY".into(), secret);
        env.insert(
            "OAUTH_SESSION_TOKEN_ENCRYPTION_KEY".into(),
            self.persistent_secret("oauth-key"),
        );
        env.insert("DATA_DIR".into(), s(&self.data_dir));
        env.insert("HF_HUB_OFFLINE".into(), "1".into());
        env.insert("TRANSFORMERS_OFFLINE".into(), "1".into());
        env.insert("HF_HOME".into(), hf.clone());
        env.insert("SENTENCE_TRANSFORMERS_HOME".into(), hf);
        env.insert("NLTK_DATA".into(), s(&assets.join("nltk")));
        env.insert("SCARF_NO_ANALYTICS".into(), "true".into());
        env.insert("DO_NOT_TRACK".into(), "true".into());
        env.insert("ANONYMIZED_TELEMETRY".into(), "False".into());
        if let Some(extra) = &self.child_ld {
            env.insert("LD_LIBRARY_PATH".into(), extra.clone());
        }

        SpawnSpec {
            program: self.python.to_string_lossy().into_owned(),
            args: vec![
                "-m".into(),
                "uvicorn".into(),
                "open_webui.main:app".into(),
                "--host".into(),
                self.host.clone(),
                "--port".into(),
                self.port.to_string(),
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
            let url = format!("{}/health", self.base_url());
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
            name: "open-webui".into(),
            host: self.host.clone(),
            tcp_port: self.port,
        }]
    }
}
