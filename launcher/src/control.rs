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

/// Build the ollama + open-webui spawn specs from the ported config/paths.
pub fn service_specs() -> Vec<(String, SpawnSpec)> {
    let ollama = spec(
        paths::ollama_binary().to_string_lossy().into_owned(),
        &["serve"],
        config::ollama_env(),
    );
    let port = config::webui_port().to_string();
    let webui = spec(
        paths::venv_python().to_string_lossy().into_owned(),
        &["-m", "uvicorn", "open_webui.main:app", "--host", config::WEBUI_HOST, "--port", &port],
        config::webui_env(),
    );
    vec![("ollama".into(), ollama), ("open-webui".into(), webui)]
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

/// Poll an HTTP health URL (GET, expect any 2xx–4xx) without an http-client dep.
pub async fn http_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let Ok(mut stream) = tokio::net::TcpStream::connect(authority).await else { return false };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = authority.split(':').next().unwrap_or(authority);
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 16];
    let Ok(n) = stream.read(&mut buf).await else { return false };
    // "HTTP/1.x NNN" — accept 2xx/3xx/4xx (service is up + answering)
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split(' ').nth(1).and_then(|c| c.parse::<u16>().ok()).map(|c| (200..500).contains(&c)).unwrap_or(false)
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
