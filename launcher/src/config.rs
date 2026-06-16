//! Child-process environment for ollama + open-webui (ported from
//! app/main/config.js). Enforces the offline-kiosk contract: no auth, no runtime
//! network fetches (embedding model + nltk data are pre-bundled).

use crate::paths;
use std::collections::HashMap;

pub const OLLAMA_HOST: &str = "127.0.0.1";
pub const WEBUI_HOST: &str = "127.0.0.1";
pub const LLMFIT_HOST: &str = "127.0.0.1";
pub const OLLAMA_PORT_DEFAULT: u16 = 11434;
pub const WEBUI_PORT_DEFAULT: u16 = 8080;
/// llmfit's model-browser API port (proxied by the launcher). Its own port in the
/// ollama-adjacent range — deliberately NOT 8787, which collides with common host
/// services (e.g. RStudio). `init_ports` falls back to an ephemeral port if taken.
pub const LLMFIT_PORT_DEFAULT: u16 = 11436;

/// The ollama / open-webui ports. Default to the well-known 11434 / 8080, but the
/// launcher may override them via PLANAI_OLLAMA_PORT / PLANAI_WEBUI_PORT when the
/// defaults are already taken (e.g. a system ollama on the host) — see
/// `init_ports`. Read everywhere so the supervisor specs, health checks, and the
/// SPA's /api/info all agree.
pub fn ollama_port() -> u16 {
    std::env::var("PLANAI_OLLAMA_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(OLLAMA_PORT_DEFAULT)
}
pub fn webui_port() -> u16 {
    std::env::var("PLANAI_WEBUI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(WEBUI_PORT_DEFAULT)
}
pub fn llmfit_port() -> u16 {
    std::env::var("PLANAI_LLMFIT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(LLMFIT_PORT_DEFAULT)
}

/// Pick free ports for ollama + open-webui (once, in the host) so the bundled
/// stack never collides with a service already on 11434/8080 (a host ollama makes
/// ours exit 1 in a crash loop). Keeps the default when it's free; otherwise asks
/// the OS for an ephemeral port. Idempotent — skips if already chosen (the
/// supervisor/FHS child inherit the env).
pub fn init_ports() {
    fn pick(env_key: &str, preferred: u16) {
        if std::env::var_os(env_key).is_some() {
            return;
        }
        use std::net::TcpListener;
        let port = if TcpListener::bind((OLLAMA_HOST, preferred)).is_ok() {
            preferred
        } else {
            TcpListener::bind((OLLAMA_HOST, 0))
                .ok()
                .and_then(|l| l.local_addr().ok())
                .map(|a| a.port())
                .unwrap_or(preferred)
        };
        std::env::set_var(env_key, port.to_string());
    }
    pick("PLANAI_OLLAMA_PORT", OLLAMA_PORT_DEFAULT);
    pick("PLANAI_WEBUI_PORT", WEBUI_PORT_DEFAULT);
    pick("PLANAI_LLMFIT_PORT", LLMFIT_PORT_DEFAULT);
}

pub fn ollama_health_url() -> String {
    format!("http://{OLLAMA_HOST}:{}/api/version", ollama_port())
}
pub fn webui_health_url() -> String {
    format!("http://{WEBUI_HOST}:{}/health", webui_port())
}
pub fn webui_url() -> String {
    format!("http://{WEBUI_HOST}:{}", webui_port())
}

fn base_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// On NixOS dev runs, nix libs for the foreign child binaries come via
/// PLANAI_CHILD_LD_LIBRARY_PATH — applied per child only, never to electron.
fn child_ld(env: &mut HashMap<String, String>) {
    if let Ok(extra) = std::env::var("PLANAI_CHILD_LD_LIBRARY_PATH") {
        if !extra.is_empty() {
            let v = match env.get("LD_LIBRARY_PATH") {
                Some(e) if !e.is_empty() => format!("{extra}:{e}"),
                _ => extra,
            };
            env.insert("LD_LIBRARY_PATH".into(), v);
        }
    }
}

fn rand_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("getrandom");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Get-or-create a persistent secret on the USB (DATA_DIR) so Open-WebUI's
/// encrypted fields stay decryptable across runs.
fn persistent_secret(name: &str) -> String {
    let f = paths::data_dir().join(format!(".{name}"));
    if let Ok(s) = std::fs::read_to_string(&f) {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    let v = rand_hex(32);
    let _ = std::fs::write(&f, &v);
    v
}

fn s(p: std::path::PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

pub fn ollama_env() -> HashMap<String, String> {
    let mut e = base_env();
    e.insert("OLLAMA_HOST".into(), format!("{OLLAMA_HOST}:{}", ollama_port()));
    e.insert("OLLAMA_MODELS".into(), s(paths::models_dir()));
    e.entry("OLLAMA_KEEP_ALIVE".into()).or_insert_with(|| "5m".into());
    child_ld(&mut e);
    e
}

pub fn webui_env() -> HashMap<String, String> {
    let assets = paths::ow_assets();
    let hf = s(assets.join("hf"));
    let mut e = base_env();
    if let Some(fe) = paths::ow_frontend_dir() {
        e.insert("FRONTEND_BUILD_DIR".into(), s(fe));
    }
    e.insert("HOST".into(), WEBUI_HOST.into());
    e.insert("PORT".into(), webui_port().to_string());
    e.insert("OLLAMA_BASE_URL".into(), format!("http://{OLLAMA_HOST}:{}", ollama_port()));
    e.insert("WEBUI_AUTH".into(), "False".into());
    e.entry("WEBUI_SECRET_KEY".into()).or_insert_with(|| persistent_secret("secret-key"));
    e.entry("OAUTH_SESSION_TOKEN_ENCRYPTION_KEY".into())
        .or_insert_with(|| persistent_secret("oauth-key"));
    e.insert("DATA_DIR".into(), s(paths::data_dir()));
    e.insert("HF_HUB_OFFLINE".into(), "1".into());
    e.insert("TRANSFORMERS_OFFLINE".into(), "1".into());
    e.insert("HF_HOME".into(), hf.clone());
    e.insert("SENTENCE_TRANSFORMERS_HOME".into(), hf);
    e.insert("NLTK_DATA".into(), s(assets.join("nltk")));
    e.insert("SCARF_NO_ANALYTICS".into(), "true".into());
    e.insert("DO_NOT_TRACK".into(), "true".into());
    e.insert("ANONYMIZED_TELEMETRY".into(), "False".into());
    child_ld(&mut e);
    e
}
