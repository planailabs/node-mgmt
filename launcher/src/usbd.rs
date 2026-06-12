//! Spawn the plan.ai USB daemon (`mac-mgmt usbd`) as the control plane.
//!
//! When a usbd binary is shipped (a launcher component, or `PLANAI_USBD_BIN`),
//! the launcher hands service ownership to the daemon: the daemon runs the
//! `mac-mgmt-services` supervisor + the spawn-from-mount services AND adds
//! heartbeat / probe / relay / config-sync. The launcher then connects to the
//! daemon's supervisor socket for `/api/status` and proxies `/api/config*` to its
//! control port (`PLANAI_USBD_URL`).
//!
//! When no usbd binary is present (plain `cargo`/dev builds), the caller falls
//! back to the launcher's own supervisor (`control::start_stack`), so existing
//! local iteration is unchanged.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use crate::log;

/// Resolve the usbd binary: `PLANAI_USBD_BIN`, else `<PLANAI_RESOURCES>/usbd/`.
pub fn resolve_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLANAI_USBD_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let res = std::env::var_os("PLANAI_RESOURCES").map(PathBuf::from)?;
    let name = if cfg!(windows) { "mac-mgmt.exe" } else { "mac-mgmt" };
    for c in [
        res.join("usbd").join(name),
        res.join("usbd").join("bin").join(name),
    ] {
        if c.exists() {
            return Some(c);
        }
    }
    None
}

/// The daemon's supervisor socket under `home` (matches
/// `mac_mgmt_services::default_socket_path()` with `HOME=home`).
pub fn socket_path(home: &Path) -> PathBuf {
    home.join(".config/mac-mgmt/services.sock")
}

fn pick_port() -> u16 {
    TcpListener::bind(("::1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(0)
}

fn set_if_unset(key: &str, val: PathBuf) {
    if std::env::var_os(key).is_none() {
        // SAFETY: called single-threaded during pre-runtime startup.
        unsafe {
            std::env::set_var(key, val);
        }
    }
}

/// Export the resolved mount paths the daemon's `Resources::from_env` consumes,
/// so the daemon doesn't re-implement the launcher's path discovery.
fn export_paths() {
    set_if_unset("PLANAI_OLLAMA_BIN", crate::paths::ollama_binary());
    set_if_unset("PLANAI_WEBUI_PYTHON", crate::paths::venv_python());
    if let Some(fe) = crate::paths::ow_frontend_dir() {
        set_if_unset("PLANAI_OW_FRONTEND", fe);
    }
    set_if_unset("PLANAI_OW_ASSETS", crate::paths::ow_assets());
    set_if_unset("PLANAI_MODELS_DIR", crate::paths::models_dir());
    set_if_unset("PLANAI_DATA_DIR", crate::paths::data_dir());
    // hermes (optional feature): only exported when the component is mounted.
    let hermes = crate::paths::resources_root().join("hermes");
    if hermes.is_dir() {
        set_if_unset("PLANAI_HERMES_PYTHON", crate::paths::hermes_python());
        set_if_unset("PLANAI_HERMES_WEB_DIST", hermes.join("share").join("web_dist"));
    }
    // hermes-webui (same feature): pure sources, run with the hermes python.
    let hermes_webui = crate::paths::resources_root().join("hermes-webui");
    if hermes_webui.is_dir() {
        set_if_unset("PLANAI_HERMES_WEBUI_DIR", hermes_webui);
    }
    // llama.cpp (optional feature): the launcher mounted the GPU-detected flavour.
    if crate::paths::resources_root().join("llamacpp").is_dir() {
        set_if_unset("PLANAI_LLAMACPP_BIN", crate::paths::llamacpp_server());
    }
}

/// Seed `<home>/config.json` enabling ollama + open-webui (with the preferred
/// ports) when no config exists, so a fresh stick brings the services up. An
/// existing config (user-edited via the Config tab) is left untouched.
fn seed_config(home: &Path, ollama_port: u16, webui_port: u16) {
    let sel = crate::update::read_selection();
    let feature_on = |f: &str| sel.features.iter().any(|x| x == f);
    // The launcher owns these enabled-flags (they mirror the drive's feature
    // selection + what's actually mounted); everything else in the config stays
    // the user's.
    let openwebui_on = feature_on("openwebui") && crate::paths::venv_python().exists();
    let hermes_on = feature_on("hermes") && crate::paths::hermes_python().exists();
    let hermes_webui_on =
        hermes_on && crate::paths::resources_root().join("hermes-webui").join("bootstrap.py").exists();
    let llamacpp_on = feature_on("llamacpp") && crate::paths::llamacpp_server().exists();

    let json = home.join("config.json");
    let _ = std::fs::create_dir_all(home);
    if !json.exists() && !home.join("config.toml").exists() {
        let cfg = serde_json::json!({
            "ollama": { "enabled": true, "port": ollama_port },
            "openwebui": { "enabled": openwebui_on, "port": webui_port },
            "hermes": { "enabled": hermes_on, "webui_enabled": hermes_webui_on },
            "llamacpp": { "enabled": llamacpp_on },
        });
        if let Ok(s) = serde_json::to_string_pretty(&cfg) {
            let _ = std::fs::write(&json, s);
        }
        return;
    }
    // Existing config: sync ONLY the feature-driven enabled flags (a toggled
    // feature must take effect on the next launch without nuking user edits).
    if let Ok(txt) = std::fs::read_to_string(&json) {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(obj) = v.as_object_mut() {
                obj.entry("openwebui").or_insert_with(|| serde_json::json!({}))["enabled"] =
                    serde_json::json!(openwebui_on);
                {
                    let h = obj.entry("hermes").or_insert_with(|| serde_json::json!({}));
                    h["enabled"] = serde_json::json!(hermes_on);
                    h["webui_enabled"] = serde_json::json!(hermes_webui_on);
                }
                obj.entry("llamacpp").or_insert_with(|| serde_json::json!({}))["enabled"] =
                    serde_json::json!(llamacpp_on);
                if let Ok(s) = serde_json::to_string_pretty(&v) {
                    let _ = std::fs::write(&json, s);
                }
            }
        }
    }
}

/// Spawn the daemon and set `PLANAI_USBD_URL`. Returns the daemon's home,
/// supervisor socket, and child handle so the caller can connect a status
/// `Client` and later stop the daemon (see `stop`).
///
/// Call BEFORE the tokio runtime (single-threaded) — it sets env the daemon
/// child inherits.
pub fn spawn(bin: &Path) -> Option<(PathBuf, PathBuf, Child)> {
    let home = crate::cache_root().join("usbd-home");
    if let Err(e) = std::fs::create_dir_all(&home) {
        log(&format!("usbd: create home failed: {e}"));
        return None;
    }
    export_paths();
    seed_config(&home, crate::config::ollama_port(), crate::config::webui_port());

    // The daemon's networked parts (heartbeat/relay/sync) follow the drive's
    // "mgmt" feature — a runtime toggle (was: the compile-time `future` flag).
    if crate::update::read_selection().features.iter().any(|f| f == "mgmt") {
        // SAFETY: single-threaded startup, before the tokio runtime.
        unsafe { std::env::set_var("USBD_NETWORKED", "1") };
    }

    let port = pick_port();
    // SAFETY: single-threaded startup, before the tokio runtime.
    unsafe {
        std::env::set_var("PLANAI_USBD_URL", format!("http://[::1]:{port}"));
    }

    let mut cmd = Command::new(bin);
    cmd.arg("usbd")
        .arg("--home")
        .arg(&home)
        .arg("--control-port")
        .arg(port.to_string());
    match cmd.spawn() {
        Ok(child) => {
            log(&format!(
                "usbd: spawned daemon (home={}, control=http://[::1]:{port})",
                home.display()
            ));
            Some((home.clone(), socket_path(&home), child))
        }
        Err(e) => {
            log(&format!("usbd: spawn failed: {e}"));
            None
        }
    }
}

/// Stop the daemon before the launcher unmounts the component pool.
///
/// The daemon's SIGTERM handler shuts down its managed services (ollama /
/// open-webui / memvault — which run from the mounted runtime/ollama trees) and
/// then exits, releasing every reference into the mounts (incl. the daemon's own
/// executable, mmap'd from the usbd mount). We wait for it so the subsequent
/// `fusermount -u` / `hdiutil detach` succeeds on the first try instead of
/// hitting "device busy" and falling back to a lazy unmount. Hard-kills as a last
/// resort if it doesn't exit within the grace window.
pub fn stop(mut child: Child) {
    // Already gone?
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    {
        // SAFETY: `child` is our direct descendant; SIGTERM asks for graceful
        // shutdown (the daemon stops its services, then exits).
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    // Wait for graceful service teardown (the supervisor SIGTERMs each service,
    // then SIGKILLs after its own grace period), bounded so we never hang here.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                log("usbd: daemon stopped");
                return;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            _ => {
                log("usbd: daemon did not exit in time — killing");
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}
