//! USB-daemon config loading.
//!
//! Loads the reduced [`UsbConfig`] from `<home>/config.{json,toml}`, then —
//! when a `[server]` url+token is configured and we're online — OPTIONALLY syncs
//! the cluster config from the management server (`GET /api/config`), merging the
//! local file on top and deserializing into `UsbConfig`. Because `UsbConfig` is a
//! subset that does NOT use `deny_unknown_fields`, a full cluster config drops
//! straight in and only the subset sections are honoured. The remote response is
//! cached on the stick so the daemon still boots when the server is unreachable.
//!
//! Reuses the daemon's existing fetch/merge helpers
//! (`mac_mgmt_agent::config::{fetch_remote_config, fetch_secrets, merge_json}`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crate::usb_config::UsbConfig;

/// Config read candidates, in priority order: explicit `--config`,
/// `<home>/config.json` (UI-editable), `<home>/config.toml`.
pub fn config_read_candidates(home: &Path, explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        v.push(p.to_path_buf());
    }
    v.push(home.join("config.json"));
    v.push(home.join("config.toml"));
    v
}

/// Where the UI / `PUT /config` writes config: `<home>/config.json`.
pub fn config_write_path(home: &Path) -> PathBuf {
    home.join("config.json")
}

fn remote_cache_path(home: &Path) -> PathBuf {
    home.join(".remote-config.json")
}

/// Parse a config file by extension (`.json` → JSON, else TOML) into a raw
/// migrated JSON value. `None` if it doesn't exist or fails to parse.
fn read_stored_value(path: &Path) -> Option<serde_json::Value> {
    let contents = std::fs::read_to_string(path).ok()?;
    let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
    let mut v: serde_json::Value = if is_json {
        serde_json::from_str(&contents).ok()?
    } else {
        toml::from_str::<toml::Value>(&contents)
            .ok()
            .and_then(|t| serde_json::to_value(t).ok())?
    };
    mac_mgmt_common::config_migrate::migrate(&mut v);
    Some(v)
}

/// The first existing config file as a raw stored JSON value (only the values the
/// user set; `{}` when none). Used by the control server's `GET /config` so the
/// editor sees stored values, not a full defaults dump.
pub fn read_stored_config(home: &Path, explicit: Option<&Path>) -> serde_json::Value {
    for p in config_read_candidates(home, explicit) {
        if let Some(v) = read_stored_value(&p) {
            return v;
        }
    }
    serde_json::json!({})
}

/// Resolve a possibly-`env:`-prefixed token against `.env`.
fn resolve_token(
    raw: &str,
    env_vars: &std::collections::HashMap<String, String>,
) -> String {
    if let Some(var) = raw.strip_prefix("env:") {
        env_vars.get(var).cloned().unwrap_or_else(|| raw.to_string())
    } else {
        raw.to_string()
    }
}

/// Load the USB config: local file, then optional remote subset sync.
///
/// `offline` skips all network access (the on-stick config + cache only).
pub async fn load(home: &Path, explicit: Option<&Path>, offline: bool) -> Result<UsbConfig> {
    // 1. Local file → raw JSON (so we can merge a remote base under it).
    let local_value: Option<serde_json::Value> = config_read_candidates(home, explicit)
        .iter()
        .find_map(|p| read_stored_value(p));

    let env_vars = mac_mgmt_agent::config::load_env_file();

    // Remote config sync is a "network part" — gated behind the runtime
    // `USBD_NETWORKED` flag (the launcher's `mgmt` feature). Without it (the
    // default), only the on-stick config file is honoured.
    let net = !offline && super::networked();

    // 2. Server coordinates (from the local file) drive the optional sync.
    let server_url = local_value
        .as_ref()
        .and_then(|v| v.get("server")?.get("url")?.as_str())
        .map(String::from);
    let server_token = local_value
        .as_ref()
        .and_then(|v| v.get("server")?.get("token")?.as_str())
        .map(|s| resolve_token(s, &env_vars));

    // 3. Secrets vault (optional) for env:/secret: resolution.
    let vault = if let (true, Some(url), Some(token)) =
        (net, server_url.as_deref(), server_token.as_deref())
    {
        mac_mgmt_agent::config::fetch_secrets(url, token).await
    } else {
        mac_mgmt_agent::secrets_cache::load_cached_secrets().unwrap_or_default()
    };

    // 4. Optional remote config sync (subset honoured automatically).
    let mut cfg = if let (true, Some(url), Some(token)) =
        (net, server_url.as_deref(), server_token.as_deref())
    {
        match mac_mgmt_agent::config::fetch_remote_config(url, token).await {
            Ok(Some(mut remote)) => {
                mac_mgmt_common::config_migrate::migrate(&mut remote);
                if let Err(e) = cache_remote(home, &remote) {
                    tracing::warn!("failed to cache remote config: {e}");
                }
                merge_and_parse(remote, local_value.clone())
            }
            Ok(None) => {
                tracing::info!("server has no config (404); using local + cache");
                from_cache_or_local(home, local_value.clone())
            }
            Err(e) => {
                tracing::warn!("remote config fetch failed: {e}; using cache/local");
                from_cache_or_local(home, local_value.clone())
            }
        }
    } else {
        from_cache_or_local(home, local_value.clone())
    };

    // 5. Resolve env:/secret: references in Secret fields.
    if let Err(errors) = cfg.resolve_secrets(&env_vars, &vault) {
        for e in &errors {
            tracing::warn!("secret resolution failed: {e}");
        }
    }

    Ok(cfg)
}

/// Merge the local file (overlay) on top of the remote base, then deserialize
/// into `UsbConfig`. Falls back to the local file alone on parse failure.
fn merge_and_parse(mut base: serde_json::Value, local: Option<serde_json::Value>) -> UsbConfig {
    if let Some(local) = &local {
        mac_mgmt_agent::config::merge_json(&mut base, local);
        mac_mgmt_common::config_migrate::migrate(&mut base);
    }
    match serde_json::from_value::<UsbConfig>(base) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("remote+local config invalid ({e}); using local only");
            parse_local(local)
        }
    }
}

/// Use the cached remote config (merged with local) if present, else the local
/// file alone, else defaults.
fn from_cache_or_local(home: &Path, local: Option<serde_json::Value>) -> UsbConfig {
    if let Some(cached) = read_stored_value(&remote_cache_path(home)) {
        tracing::info!("using cached remote config");
        return merge_and_parse(cached, local);
    }
    parse_local(local)
}

fn parse_local(local: Option<serde_json::Value>) -> UsbConfig {
    match local {
        Some(v) => serde_json::from_value::<UsbConfig>(v).unwrap_or_else(|e| {
            tracing::warn!("local config invalid ({e}); using defaults");
            UsbConfig::default()
        }),
        None => {
            tracing::info!("no config.json/config.toml found; using defaults");
            UsbConfig::default()
        }
    }
}

fn cache_remote(home: &Path, json: &serde_json::Value) -> Result<()> {
    let path = remote_cache_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let s = serde_json::to_string(json).context("serialize remote config")?;
    std::fs::write(&path, s).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
