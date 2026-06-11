//! Rust control plane (ported from app/main/supervisor.js): spawn the
//! mac-mgmt-services supervisor, register ollama + open-webui, and health-check
//! them. The supervisor itself spawns/restarts the processes; this drives it.

use crate::{config, paths};
use anyhow::{Context, Result};
use mac_mgmt_services::{Client, SpawnSpec};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

fn spec(program: String, args: &[&str], env: HashMap<String, String>) -> SpawnSpec {
    SpawnSpec {
        program,
        args: args.iter().map(|s| s.to_string()).collect(),
        env,
    }
}

/// Build the spawn specs from the ported config/paths: ollama always; open-webui
/// and hermes per the drive's feature selection (this is the DEV FALLBACK path —
/// in shipped bundles the usb daemon owns the services and applies the same
/// feature gating via its seeded config).
pub fn service_specs() -> Vec<(String, SpawnSpec)> {
    let features = crate::update::read_selection().features;
    let on = |f: &str| features.iter().any(|x| x == f);
    let ollama = spec(
        paths::ollama_binary().to_string_lossy().into_owned(),
        &["serve"],
        config::ollama_env(),
    );
    let mut specs = vec![("ollama".to_string(), ollama)];
    if on("openwebui") {
        let port = config::webui_port().to_string();
        let webui = spec(
            paths::venv_python().to_string_lossy().into_owned(),
            &["-m", "uvicorn", "open_webui.main:app", "--host", config::WEBUI_HOST, "--port", &port],
            config::webui_env(),
        );
        specs.push(("open-webui".into(), webui));
    }
    if on("hermes") && paths::hermes_python().exists() {
        let home = paths::data_dir().join("hermes");
        let mut env: std::collections::HashMap<String, String> = HashMap::new();
        env.insert("HERMES_HOME".into(), home.to_string_lossy().into_owned());
        env.insert(
            "HERMES_WEB_DIST".into(),
            paths::resources_root().join("hermes").join("share").join("web_dist").to_string_lossy().into_owned(),
        );
        env.insert("HERMES_MANAGED".into(), "1".into());
        let hermes = spec(
            paths::hermes_python().to_string_lossy().into_owned(),
            &["-m", "hermes_cli.main", "dashboard", "--host", "127.0.0.1", "--port", "9119", "--no-open", "--skip-build"],
            env,
        );
        specs.push(("hermes".into(), hermes));
    }
    specs
}

/// Start the supervisor (a re-exec of `self_exe`) and register the services.
/// Returns the connected client (keep it alive to receive logs / query status).
pub async fn start_stack(self_exe: &Path, socket: &Path) -> Result<Client> {
    tokio::process::Command::new(self_exe)
        .arg("supervisor")
        .arg(socket)
        .spawn()
        .context("spawn supervisor subprocess")?;

    let mut client = Client::connect(socket, Duration::from_secs(15))
        .await
        .context("connect to supervisor")?;

    for (name, spec) in service_specs() {
        client
            .register(&name, spec)
            .await
            .with_context(|| format!("register {name}"))?;
    }
    Ok(client)
}

/// Poll an HTTP health URL (GET): up if it answers with any 2xx/3xx/4xx.
pub async fn http_ok(url: &str) -> bool {
    match crate::net::client().get(url).timeout(Duration::from_secs(5)).send().await {
        Ok(r) => (200..500).contains(&r.status().as_u16()),
        Err(_) => false,
    }
}

/// Wait until both services answer their health endpoint (or `timeout`).
pub async fn await_healthy(timeout: Duration) -> (bool, bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    let (mut ollama, mut webui) = (false, false);
    while tokio::time::Instant::now() < deadline && !(ollama && webui) {
        if !ollama {
            ollama = http_ok(&config::ollama_health_url()).await;
        }
        if !webui {
            webui = http_ok(&config::webui_health_url()).await;
        }
        if ollama && webui {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    (ollama, webui)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_well_formed() {
        std::env::set_var("PLANAI_RESOURCES", "/tmp/planai-test-res");
        let specs = service_specs();
        assert_eq!(specs.len(), 2);
        let (oname, ollama) = &specs[0];
        assert_eq!(oname, "ollama");
        assert!(ollama.program.ends_with("ollama") || ollama.program.ends_with("ollama.exe"));
        assert_eq!(ollama.args, vec!["serve"]);
        assert_eq!(ollama.env.get("OLLAMA_HOST").map(String::as_str), Some("127.0.0.1:11434"));
        let (wname, webui) = &specs[1];
        assert_eq!(wname, "open-webui");
        assert_eq!(webui.args[0], "-m");
        assert_eq!(webui.args[1], "uvicorn");
        assert_eq!(webui.env.get("WEBUI_AUTH").map(String::as_str), Some("False"));
        assert!(webui.env.contains_key("WEBUI_SECRET_KEY"));
    }
}
