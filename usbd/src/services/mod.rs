//! Spawn-from-mount services for the USB daemon.
//!
//! Each service is a proper [`ManagedService`] (the daemon's own idiom — see
//! `daemon/src/services/ollama.rs`), NOT a synthesized custom-service. The
//! binaries are run directly from the dmg/squashfs the launcher already mounted
//! (`PLANAI_RESOURCES`), so `ensure_installed`/`ensure_setup` are no-ops: there
//! is no nix install or upgrade on the stick.
//!
//! The **effective port** is resolved once in each service's `new()` (see
//! [`resolve_port`]) and owned by the service, so spawn / health / tunnel /
//! inventory can never disagree.

pub mod hermes_dashboard;
pub mod hermes_gateway;
pub mod hermes_webui;
pub mod llamacpp;
pub mod memvault;
pub mod ollama;
pub mod open_webui;

use std::path::PathBuf;
use std::sync::Arc;

use crate::usb_config::UsbConfig;

use mac_mgmt_agent::managed_service::ManagedService;

/// Resolved paths + flags for the mounted components. These are normally
/// **exported by the launcher** (which owns `PLANAI_RESOURCES` and the path
/// resolution); the `$PLANAI_RESOURCES/...` fallbacks let the daemon run
/// standalone (tests, `--home` dev runs).
#[derive(Clone, Debug)]
pub struct Resources {
    /// `<resources>/ollama[/bin]/ollama` — the ollama binary.
    pub ollama_bin: PathBuf,
    /// The python interpreter that runs `uvicorn open_webui.main:app`.
    pub webui_python: PathBuf,
    /// Open-WebUI's installed frontend dir (`FRONTEND_BUILD_DIR`), if known.
    pub ow_frontend: Option<PathBuf>,
    /// `<resources>/ow-assets` — HF/NLTK offline assets.
    pub ow_assets: PathBuf,
    /// Writable models dir (`OLLAMA_MODELS`).
    pub models_dir: PathBuf,
    /// Writable data dir (`DATA_DIR` for open-webui sqlite/uploads).
    pub data_dir: PathBuf,
    /// Per-child `LD_LIBRARY_PATH` prefix (NixOS dev runs); empty otherwise.
    pub child_ld_library_path: Option<String>,
    /// `<resources>/hermes/python/...` — the hermes component's python (the
    /// optional "hermes" feature; only used when `hermes.enabled`).
    pub hermes_python: PathBuf,
    /// `<resources>/hermes/share/web_dist` — the hermes dashboard SPA.
    pub hermes_web_dist: PathBuf,
    /// `<resources>/hermes-webui` — the hermes web UI component (pure python/
    /// static sources, run with the hermes component's python).
    pub hermes_webui_dir: PathBuf,
    /// The mounted llama.cpp component's `llama-server` (optional "llamacpp"
    /// feature; GPU flavour picked by the launcher).
    pub llamacpp_bin: PathBuf,
}

impl Resources {
    /// Build from the launcher-exported `PLANAI_*` env, falling back to the
    /// standard `$PLANAI_RESOURCES` component layout.
    pub fn from_env() -> Self {
        let res_root = std::env::var_os("PLANAI_RESOURCES")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("dist"));
        let portable_root = std::env::var_os("PLANAI_PORTABLE_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|p| p.to_path_buf()))
            })
            .unwrap_or_else(|| PathBuf::from("."));

        let env_path = |key: &str| std::env::var_os(key).map(PathBuf::from);

        let ollama_name = if cfg!(windows) { "ollama.exe" } else { "ollama" };
        let ollama_bin = env_path("PLANAI_OLLAMA_BIN").unwrap_or_else(|| {
            let dir = res_root.join("ollama");
            let nested = dir.join("bin").join(ollama_name);
            if nested.exists() {
                nested
            } else {
                dir.join(ollama_name)
            }
        });

        let webui_python = env_path("PLANAI_WEBUI_PYTHON").unwrap_or_else(|| {
            let rt = res_root.join("runtime");
            if cfg!(windows) {
                rt.join("python").join("python.exe")
            } else {
                rt.join("python").join("bin").join("python3")
            }
        });

        let hermes_root = res_root.join("hermes");
        let hermes_python = env_path("PLANAI_HERMES_PYTHON").unwrap_or_else(|| {
            if cfg!(windows) {
                hermes_root.join("python").join("python.exe")
            } else {
                hermes_root.join("python").join("bin").join("python3")
            }
        });
        let hermes_web_dist = env_path("PLANAI_HERMES_WEB_DIST")
            .unwrap_or_else(|| hermes_root.join("share").join("web_dist"));
        let hermes_webui_dir = env_path("PLANAI_HERMES_WEBUI_DIR")
            .unwrap_or_else(|| res_root.join("hermes-webui"));

        let server_name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
        let llamacpp_bin = env_path("PLANAI_LLAMACPP_BIN").unwrap_or_else(|| {
            let root = res_root.join("llamacpp");
            // upstream archives differ: linux/mac pack build/bin/, win is flat
            for c in [root.join("build").join("bin").join(server_name), root.join("bin").join(server_name)] {
                if c.exists() {
                    return c;
                }
            }
            root.join(server_name)
        });

        Self {
            ollama_bin,
            webui_python,
            ow_frontend: env_path("PLANAI_OW_FRONTEND"),
            ow_assets: env_path("PLANAI_OW_ASSETS")
                .unwrap_or_else(|| res_root.join("ow-assets")),
            models_dir: env_path("PLANAI_MODELS_DIR")
                .unwrap_or_else(|| portable_root.join("models")),
            data_dir: env_path("PLANAI_DATA_DIR")
                .unwrap_or_else(|| portable_root.join("data")),
            child_ld_library_path: std::env::var("PLANAI_CHILD_LD_LIBRARY_PATH")
                .ok()
                .filter(|s| !s.is_empty()),
            hermes_python,
            hermes_web_dist,
            hermes_webui_dir,
            llamacpp_bin,
        }
    }
}

/// Resolve the effective listen port for `host`. The configured `preferred`
/// port is honoured when free; on collision the OS picks a free ephemeral port.
///
/// The probe listener is dropped immediately — the service rebinds microseconds
/// later. This TOCTOU window is acceptable on a single-host stick (nothing else
/// is racing for the port) and far better than crash-looping on a taken port.
pub fn resolve_port(host: &str, preferred: u16) -> u16 {
    use std::net::TcpListener;
    if TcpListener::bind((host, preferred)).is_ok() {
        return preferred;
    }
    tracing::warn!("port {host}:{preferred} is in use; auto-selecting a free port");
    TcpListener::bind((host, 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(preferred)
}

/// Write a secret file (WEBUI_SECRET_KEY, oauth key, gateway API key, …) readable
/// only by its owner. On FAT32 (the shipped USB) the mode is a harmless no-op — no
/// unix perms there — but when the daemon's data dir lives on a real filesystem
/// these keys would otherwise land world-readable (0644) for any other local user.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes) {
                    tracing::warn!("secret write {}: {e}", path.display());
                }
            }
            Err(e) => tracing::warn!("secret open {}: {e}", path.display()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(path, bytes);
    }
}

/// The effective (bound) ports, reported in heartbeat + the control `/info`
/// endpoint so the dashboard links to the right place even after a collision.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResolvedPorts {
    pub ollama: Option<u16>,
    pub openwebui: Option<u16>,
    pub memvault: Option<u16>,
    pub hermes: Option<u16>,
    pub hermes_gateway: Option<u16>,
    pub hermes_webui: Option<u16>,
    pub llamacpp: Option<u16>,
}

/// Build the enabled spawn-from-mount services from the reduced config and the
/// resolved mount paths. Returns the services plus the **effective** ports.
///
/// Memvault is NOT returned here: its store + web app run in-process via
/// `MemvaultHandle` in `run_stack` (see `usb.rs::maybe_serve_memvault`). A thin
/// integrated health service is added there when its port is known.
pub fn build_usb_services(
    cfg: &UsbConfig,
    res: &Resources,
) -> (Vec<Arc<dyn ManagedService>>, ResolvedPorts) {
    let mut services: Vec<Arc<dyn ManagedService>> = Vec::new();
    let mut ports = ResolvedPorts::default();

    if cfg.ollama.enabled {
        let svc = ollama::UsbOllamaService::new(&cfg.ollama, res);
        ports.ollama = Some(svc.port());
        services.push(Arc::new(svc));
    }
    // llamacpp before open-webui so its port is known: open-webui registers the
    // router as an OpenAI provider. 127.0.0.1 (always local) regardless of bind host.
    if cfg.llamacpp.enabled {
        let svc = llamacpp::UsbLlamaCppService::new(&cfg.llamacpp, res);
        ports.llamacpp = Some(svc.port());
        services.push(Arc::new(svc));
    }
    if cfg.openwebui.enabled {
        let llamacpp_openai_url = ports.llamacpp.map(|p| format!("http://127.0.0.1:{p}/v1"));
        let svc = open_webui::UsbOpenWebuiService::new(&cfg.openwebui, &cfg.ollama, res, ports.ollama, llamacpp_openai_url);
        ports.openwebui = Some(svc.port());
        services.push(Arc::new(svc));
    }
    if cfg.hermes.enabled {
        let svc = hermes_dashboard::UsbHermesDashboardService::new(
            &cfg.hermes,
            &cfg.ollama,
            res,
            ports.ollama,
        );
        ports.hermes = Some(svc.port());
        services.push(Arc::new(svc));
    }
    if cfg.hermes.enabled && cfg.hermes.gateway_enabled {
        let svc = hermes_gateway::UsbHermesGatewayService::new(&cfg.hermes, res);
        ports.hermes_gateway = Some(svc.port());
        services.push(Arc::new(svc));
    }
    if cfg.hermes.enabled && cfg.hermes.webui_enabled {
        if res.hermes_webui_dir.join("bootstrap.py").exists() {
            let svc = hermes_webui::UsbHermesWebuiService::new(&cfg.hermes, res);
            ports.hermes_webui = Some(svc.port());
            services.push(Arc::new(svc));
        } else {
            tracing::warn!(
                dir = %res.hermes_webui_dir.display(),
                "hermes.webui_enabled but the hermes-webui component is not mounted — skipping"
            );
        }
    }
    (services, ports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::path::PathBuf;

    fn test_resources() -> Resources {
        Resources {
            ollama_bin: PathBuf::from("/mnt/res/ollama/ollama"),
            webui_python: PathBuf::from("/mnt/res/runtime/python/bin/python3"),
            ow_frontend: None,
            ow_assets: PathBuf::from("/mnt/res/ow-assets"),
            models_dir: PathBuf::from("/data/models"),
            data_dir: PathBuf::from("/data/webui"),
            child_ld_library_path: None,
            hermes_python: PathBuf::from("/mnt/res/hermes/python/bin/python3"),
            hermes_webui_dir: PathBuf::from("/mnt/res/hermes-webui"),
            hermes_web_dist: PathBuf::from("/mnt/res/hermes/share/web_dist"),
            llamacpp_bin: PathBuf::from("/mnt/res/llamacpp/build/bin/llama-server"),
        }
    }

    #[test]
    fn resolve_port_keeps_free_preferred() {
        // An unbound high port should come back unchanged.
        assert_eq!(resolve_port("127.0.0.1", 54_321), 54_321);
    }

    #[test]
    fn resolve_port_falls_back_on_collision() {
        // Occupy a port, then ask for it: must get a different, bindable one.
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = occupied.local_addr().unwrap().port();
        let got = resolve_port("127.0.0.1", taken);
        assert_ne!(got, taken, "should not return the occupied port");
        // The returned port must itself be bindable.
        assert!(TcpListener::bind(("127.0.0.1", got)).is_ok());
    }

    #[test]
    fn builds_only_enabled_services() {
        let mut cfg = UsbConfig::default();
        cfg.ollama.enabled = true;
        cfg.ollama.port = 54_322;
        cfg.openwebui.enabled = true;
        cfg.openwebui.port = 54_323;
        let (svcs, ports) = build_usb_services(&cfg, &test_resources());
        let names: Vec<&str> = svcs.iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["ollama", "open-webui"]);
        assert_eq!(ports.ollama, Some(54_322));
        assert_eq!(ports.openwebui, Some(54_323));
        assert_eq!(ports.memvault, None);
    }

    #[test]
    fn disabled_services_are_skipped() {
        let cfg = UsbConfig::default(); // all disabled
        let (svcs, ports) = build_usb_services(&cfg, &test_resources());
        assert!(svcs.is_empty());
        assert_eq!(ports.ollama, None);
        assert_eq!(ports.openwebui, None);
    }
}
