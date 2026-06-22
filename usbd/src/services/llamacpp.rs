//! llama-server, run from the mounted llama.cpp component (no nix). The
//! launcher picks the GPU flavour (vulkan vs cpu; Metal on mac) and exports
//! `PLANAI_LLAMACPP_BIN`.
//!
//! Two modes (`UsbLlamaCppConfig`):
//!   - `model` set → pin that single GGUF (`-m <path>`).
//!   - `model` unset → ROUTER mode over `models_dir` (default `<models>/gguf`):
//!     `llama-server` lists every `*.gguf` there at `/v1/models` (id = filename
//!     stem) and loads/swaps them on demand, so an OpenAI request's `model`
//!     field picks which one to use. `--models-max` bounds how many stay
//!     resident. This dir is kept SEPARATE from the ollama store (whose weights
//!     are content-addressed blobs that a `--models-dir` scan can't name).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::Result;
use crate::usb_config::UsbLlamaCppConfig;

use super::{resolve_port, Resources};
use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};

pub struct UsbLlamaCppService {
    host: String,
    /// Effective (resolved) port.
    port: u16,
    bin: PathBuf,
    /// Router-mode models dir (default `<models>/gguf`).
    models_dir: PathBuf,
    /// Pin a single model instead of router mode.
    model: Option<String>,
    /// Router: max models resident at once (0 = unlimited).
    models_max: u16,
    extra_args: Vec<String>,
    child_ld: Option<String>,
}

impl UsbLlamaCppService {
    pub fn new(cfg: &UsbLlamaCppConfig, res: &Resources) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.port);
        let models_dir = cfg
            .models_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| res.models_dir.join("gguf"));
        Self {
            host,
            port,
            bin: res.llamacpp_bin.clone(),
            models_dir,
            model: cfg.model.clone(),
            models_max: cfg.models_max,
            extra_args: cfg.extra_args.clone(),
            child_ld: res.child_ld_library_path.clone(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// A pinned single GGUF (config `model`), if it exists. `None` → router mode.
    fn pinned_model(&self) -> Option<PathBuf> {
        let m = self.model.as_ref()?;
        let p = PathBuf::from(m);
        if p.exists() {
            return Some(p);
        }
        tracing::warn!("llamacpp.model {m} not found — falling back to router mode over {}", self.models_dir.display());
        None
    }
}

impl ManagedService for UsbLlamaCppService {
    fn name(&self) -> &str {
        "llamacpp"
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Managed
    }

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
        // The shared libs (libllama, ggml backends) sit BESIDE llama-server in
        // the mounted component — point the loader there (windows searches the
        // exe dir natively; mac also honours the fallback path).
        let bin_dir = self.bin.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let ld = match &self.child_ld {
            Some(extra) => format!("{bin_dir}:{extra}"),
            None => bin_dir.clone(),
        };
        env.insert("LD_LIBRARY_PATH".into(), ld);
        if cfg!(target_os = "macos") {
            env.insert("DYLD_FALLBACK_LIBRARY_PATH".into(), bin_dir);
        }
        let mut args = vec![
            "--host".to_string(),
            self.host.clone(),
            "--port".to_string(),
            self.port.to_string(),
        ];
        match self.pinned_model() {
            // single pinned model
            Some(model) => {
                args.push("-m".into());
                args.push(model.to_string_lossy().into_owned());
            }
            // router mode: serve every *.gguf in models_dir, selectable per-request
            // by the `model` field (= filename stem); load/swap up to models_max.
            None => {
                let _ = std::fs::create_dir_all(&self.models_dir);
                args.push("--models-dir".into());
                args.push(self.models_dir.to_string_lossy().into_owned());
                args.push("--models-max".into());
                args.push(self.models_max.to_string());
            }
        }
        args.extend(self.extra_args.iter().cloned());

        SpawnSpec {
            program: self.bin.to_string_lossy().into_owned(),
            args,
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
            match tokio::time::timeout(std::time::Duration::from_secs(5), client.get(&url).send()).await {
                Ok(Ok(resp)) => Ok(resp.status().is_success()),
                _ => Ok(false),
            }
        })
    }

    fn expose_tunnels(&self) -> Vec<TunnelDef> {
        vec![TunnelDef {
            name: "llamacpp".into(),
            host: self.host.clone(),
            tcp_port: self.port,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(model: Option<String>, models_dir: PathBuf) -> UsbLlamaCppService {
        UsbLlamaCppService {
            host: "127.0.0.1".into(),
            port: 8090,
            bin: PathBuf::from("/mnt/res/llamacpp/build/bin/llama-server"),
            models_dir,
            model,
            models_max: 1,
            extra_args: vec!["--ctx-size".into(), "8192".into()],
            child_ld: None,
        }
    }

    #[test]
    fn router_mode_when_model_unset() {
        let dir = std::env::temp_dir().join(format!("llamacpp-router-{}", std::process::id()));
        let spec = svc(None, dir.clone()).spawn_spec();
        assert!(spec.program.ends_with("llama-server"));
        assert!(spec.args.contains(&"--ctx-size".to_string())); // extra_args passed
        // router: --models-dir <dir> --models-max 1, and NO single -m
        let i = spec.args.iter().position(|a| a == "--models-dir").expect("--models-dir");
        assert_eq!(spec.args[i + 1], dir.to_string_lossy());
        let j = spec.args.iter().position(|a| a == "--models-max").expect("--models-max");
        assert_eq!(spec.args[j + 1], "1");
        assert!(!spec.args.contains(&"-m".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_pin_when_model_set_and_exists() {
        // a real existing path so pinned_model() returns Some
        let f = std::env::temp_dir().join(format!("llamacpp-pin-{}.gguf", std::process::id()));
        std::fs::write(&f, b"gguf").unwrap();
        let spec = svc(Some(f.to_string_lossy().into_owned()), PathBuf::from("/unused")).spawn_spec();
        let i = spec.args.iter().position(|a| a == "-m").expect("-m");
        assert_eq!(spec.args[i + 1], f.to_string_lossy());
        // pinned → not router
        assert!(!spec.args.contains(&"--models-dir".to_string()));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn missing_pin_falls_back_to_router() {
        let dir = std::env::temp_dir().join(format!("llamacpp-fallback-{}", std::process::id()));
        let spec = svc(Some("/nonexistent/model.gguf".into()), dir.clone()).spawn_spec();
        assert!(!spec.args.contains(&"-m".to_string()));
        assert!(spec.args.contains(&"--models-dir".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
