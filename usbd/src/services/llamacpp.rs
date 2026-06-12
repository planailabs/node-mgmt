//! llama-server, run from the mounted llama.cpp component (no nix). The
//! launcher picks the GPU flavour (vulkan vs cpu; Metal on mac) and exports
//! `PLANAI_LLAMACPP_BIN`. The model defaults to the FIRST local ollama model's
//! weights blob — ollama stores plain GGUF files under models/blobs, addressed
//! by the manifest layer with mediaType `application/vnd.ollama.image.model` —
//! so a stick that pulled models for ollama serves the same weights through
//! llama-server with zero extra downloads.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
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
    models_dir: PathBuf,
    model: Option<String>,
    extra_args: Vec<String>,
    child_ld: Option<String>,
}

impl UsbLlamaCppService {
    pub fn new(cfg: &UsbLlamaCppConfig, res: &Resources) -> Self {
        let host = cfg.host.clone();
        let port = resolve_port(&host, cfg.port);
        Self {
            host,
            port,
            bin: res.llamacpp_bin.clone(),
            models_dir: res.models_dir.clone(),
            model: cfg.model.clone(),
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

    /// The GGUF to serve: the configured path, else the first local ollama
    /// model's weights blob, else the first downloaded GGUF in
    /// `<models>/gguf/` (the launcher's llmfit fallback puts HuggingFace
    /// models that aren't in the ollama registry there).
    fn resolve_model(&self) -> Option<PathBuf> {
        if let Some(m) = &self.model {
            let p = PathBuf::from(m);
            if p.exists() {
                return Some(p);
            }
            tracing::warn!("llamacpp.model {m} not found — falling back to the ollama store");
        }
        first_ollama_gguf(&self.models_dir).or_else(|| first_dir_gguf(&self.models_dir.join("gguf")))
    }
}

/// The first `*.gguf` (sorted) in a flat dir of downloaded weights.
fn first_dir_gguf(dir: &Path) -> Option<PathBuf> {
    let mut ggufs: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gguf")))
        .collect();
    ggufs.sort();
    ggufs.into_iter().next()
}

/// Find the first pulled ollama model's weights blob (a plain GGUF): walk
/// models/manifests/<registry>/<ns>/<name>/<tag> (sorted), parse the manifest
/// JSON, and resolve the `application/vnd.ollama.image.model` layer digest to
/// models/blobs/sha256-<hex>.
fn first_ollama_gguf(models_dir: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: u8) -> Option<PathBuf> {
        let mut ents: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let p = e.path();
            if p.is_dir() && depth > 0 {
                if let Some(found) = walk(&p, depth - 1) {
                    return Some(found);
                }
            } else if p.is_file() {
                return Some(p);
            }
        }
        None
    }
    // manifests/<registry>/<namespace>/<name>/<tag> = 4 levels below manifests/
    let manifest = walk(&models_dir.join("manifests"), 4)?;
    let txt = std::fs::read_to_string(&manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let digest = v.get("layers")?.as_array()?.iter().find_map(|l| {
        (l.get("mediaType")?.as_str()? == "application/vnd.ollama.image.model")
            .then(|| l.get("digest")?.as_str().map(String::from))?
    })?;
    let blob = models_dir.join("blobs").join(digest.replace(':', "-"));
    blob.exists().then_some(blob)
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
        if let Some(model) = self.resolve_model() {
            args.push("-m".into());
            args.push(model.to_string_lossy().into_owned());
        } else {
            tracing::warn!("llamacpp: no GGUF model found (config llamacpp.model unset, ollama store empty)");
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

    #[test]
    fn first_ollama_gguf_resolves_model_layer_blob() {
        let base = std::env::temp_dir().join(format!("llamacpp-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mdir = base.join("manifests/registry.ollama.ai/library/smollm2");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::create_dir_all(base.join("blobs")).unwrap();
        let manifest = serde_json::json!({
            "layers": [
                { "mediaType": "application/vnd.ollama.image.template", "digest": "sha256:aaa" },
                { "mediaType": "application/vnd.ollama.image.model", "digest": "sha256:bbb" },
            ]
        });
        std::fs::write(mdir.join("1.7b"), manifest.to_string()).unwrap();
        std::fs::write(base.join("blobs/sha256-bbb"), b"gguf").unwrap();
        assert_eq!(first_ollama_gguf(&base), Some(base.join("blobs/sha256-bbb")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn spawn_spec_serves_resolved_model() {
        let svc = UsbLlamaCppService {
            host: "127.0.0.1".into(),
            port: 8090,
            bin: PathBuf::from("/mnt/res/llamacpp/build/bin/llama-server"),
            models_dir: PathBuf::from("/nonexistent"),
            model: None,
            extra_args: vec!["--ctx-size".into(), "8192".into()],
            child_ld: None,
        };
        let spec = svc.spawn_spec();
        assert!(spec.program.ends_with("llama-server"));
        assert!(spec.args.contains(&"--port".to_string()));
        assert!(spec.args.contains(&"--ctx-size".to_string()));
        // no model found → no -m flag (llama-server then errors visibly in logs)
        assert!(!spec.args.contains(&"-m".to_string()));
    }
}
