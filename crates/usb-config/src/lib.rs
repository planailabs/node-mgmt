//! The reduced USB config types — moved here from mac-mgmt-common so usbd is
//! hermetic: config shape changes no longer need an upstream mac-mgmt commit.
//! [`UsbConfig`] stays a strict SUBSET of the daemon's full config using the
//! same field names + section types (imported from mac_mgmt_common), so a full
//! cluster config fetched from the management server still deserializes
//! straight into it (extra sections silently dropped).

use mac_mgmt_common::{
    custom_service, DaemonServerConfig, DaemonSettings, GlobalConfig, MemvaultConfig,
    MetricsConfig, NotificationsConfig, OllamaConfig, OpenWebuiConfig, RelayConfig,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_host() -> String {
    "127.0.0.1".to_string()
}

// ── Hermes agent ───────────────────────────────────────────────────────

fn default_usb_hermes_port() -> u16 {
    9119
}

/// REDUCED Hermes config for the USB stick (the optional "hermes" feature) —
/// distinct from [`HermesConfig`], the full nix-install daemon section. The
/// dashboard is run from the mounted hermes component
/// (a portable python tree), never installed — runtime settings only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbHermesConfig {
    #[schemars(description = "Whether the Hermes agent dashboard is started")]
    #[serde(default)]
    pub enabled: bool,
    #[schemars(description = "Hermes dashboard listen address")]
    #[serde(default = "default_host")]
    pub host: String,
    #[schemars(
        description = "Preferred Hermes dashboard listen port. A PREFERENCE, not a \
                       guarantee: if taken the daemon auto-selects a free port and \
                       reports the effective port via the dashboard."
    )]
    #[serde(default = "default_usb_hermes_port")]
    pub port: u16,
    #[schemars(description = "Hermes home dir (HERMES_HOME: config.yaml, sessions, overlay venv); \
                              default <data>/hermes", extend("x-advanced" = true))]
    #[serde(default)]
    pub data_dir: String,
    #[schemars(description = "Model seeded into config.yaml on first run (e.g. \"smollm2:1.7b\"); \
                              auto-detected from the local models dir when unset", extend("x-advanced" = true))]
    #[serde(default)]
    pub default_model: Option<String>,
    #[schemars(description = "Whether the Hermes web UI (three-panel sessions/chat/workspace, the \
                              hermes-webui component) is started alongside the dashboard")]
    #[serde(default)]
    pub webui_enabled: bool,
    #[schemars(
        description = "Preferred Hermes web UI listen port. A PREFERENCE, not a guarantee: \
                       if taken the daemon auto-selects a free port."
    )]
    #[serde(default = "default_usb_hermes_webui_port")]
    pub webui_port: u16,
    #[schemars(description = "Whether the Hermes messaging gateway (the API server / messaging \
                              platforms — `hermes gateway run`) is started alongside the \
                              dashboard. Off by default: the stick is offline, so the gateway \
                              only matters if you want its local HTTP API.", extend("x-advanced" = true))]
    #[serde(default)]
    pub gateway_enabled: bool,
    #[schemars(
        description = "Preferred Hermes gateway API port (API_SERVER_PORT). A PREFERENCE, not a \
                       guarantee: if taken the daemon auto-selects a free port.",
        extend("x-advanced" = true)
    )]
    #[serde(default = "default_usb_hermes_gateway_port")]
    pub gateway_port: u16,
}

fn default_usb_hermes_gateway_port() -> u16 {
    // Hermes' own default API server port (upstream HermesGatewayConfig).
    8642
}

fn default_usb_hermes_webui_port() -> u16 {
    // Adjacent to the hermes dashboard (9119). Historically dodged 8787 — the
    // launcher's old fixed llmfit port — because sharing it made the hermes-webui
    // iframe land on the llmfit model browser. llmfit now has its own dedicated
    // port (launcher config::LLMFIT_PORT_DEFAULT), so 8787 is no longer in play.
    9120
}

impl Default for UsbHermesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_host(),
            port: default_usb_hermes_port(),
            data_dir: String::new(),
            default_model: None,
            webui_enabled: false,
            webui_port: default_usb_hermes_webui_port(),
            gateway_enabled: false,
            gateway_port: default_usb_hermes_gateway_port(),
        }
    }
}

impl UsbHermesConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("hermes.port must be > 0".into());
        }
        if self.webui_port == 0 {
            return Err("hermes.webui_port must be > 0".into());
        }
        if self.gateway_port == 0 {
            return Err("hermes.gateway_port must be > 0".into());
        }
        Ok(())
    }
}

// ── llama.cpp (llama-server, USB) ──────────────────────────────────────

fn default_usb_llamacpp_port() -> u16 {
    8090
}

fn default_usb_llamacpp_models_max() -> u16 {
    1
}

/// REDUCED llama.cpp config for the USB stick (the optional "llamacpp"
/// feature): `llama-server` run from the mounted component (GPU-detected
/// flavour), runtime settings only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbLlamaCppConfig {
    #[schemars(description = "Whether llama-server is started")]
    #[serde(default)]
    pub enabled: bool,
    #[schemars(description = "llama-server listen address")]
    #[serde(default = "default_host")]
    pub host: String,
    #[schemars(
        description = "Preferred llama-server listen port. A PREFERENCE, not a \
                       guarantee: if taken the daemon auto-selects a free port and \
                       reports the effective port via the dashboard."
    )]
    #[serde(default = "default_usb_llamacpp_port")]
    pub port: u16,
    #[schemars(description = "Pin a single GGUF model path. When unset, llama-server runs in \
                              ROUTER mode over `models_dir`: every *.gguf is selectable per-request \
                              by the OpenAI `model` field (= the filename stem).", extend("x-advanced" = true))]
    #[serde(default)]
    pub model: Option<String>,
    #[schemars(description = "Router mode: directory of *.gguf models to serve (filename stem = \
                              model id). When unset, defaults to <models>/gguf — kept separate from \
                              the ollama store.", extend("x-advanced" = true))]
    #[serde(default)]
    pub models_dir: Option<String>,
    #[schemars(description = "Router mode: max models kept resident at once (0 = unlimited). 1 \
                              swaps on demand (lowest RAM); raise for faster switching at higher RAM.")]
    #[serde(default = "default_usb_llamacpp_models_max")]
    pub models_max: u16,
    #[schemars(description = "Extra llama-server arguments (advanced)", extend("x-advanced" = true))]
    #[serde(default)]
    pub extra_args: Vec<String>,
}

impl Default for UsbLlamaCppConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_host(),
            port: default_usb_llamacpp_port(),
            model: None,
            models_dir: None,
            models_max: default_usb_llamacpp_models_max(),
            extra_args: Vec::new(),
        }
    }
}

impl UsbLlamaCppConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("llamacpp.port must be > 0".into());
        }
        Ok(())
    }
}

// ── USB Config (reduced subset for the sovereign-AI USB daemon) ──────────

/// The reduced config the USB daemon honours: ollama + open-webui + memvault,
/// plus the server connection (for optional config sync), relay, metrics, and
/// daemon settings. It is a strict **subset** of [`ClusterConfig`]/[`DaemonConfig`]
/// using the **same field names + types**, so a full cluster config fetched from
/// the management server deserializes straight into this struct — the
/// non-subset sections (openclaw, hermes, lms, cloud, …) are silently dropped.
///
/// NOTE: deliberately NOT `#[serde(deny_unknown_fields)]`. That tolerance is what
/// makes "only honour the subset" automatic when syncing a full cluster config.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct UsbConfig {
    #[serde(default)]
    #[schemars(extend("x-category" = "infra"))]
    pub daemon: DaemonSettings,
    #[serde(default)]
    #[schemars(extend("x-category" = "ops"))]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "identity", "x-always-on" = true))]
    pub global: GlobalConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "llm-providers"))]
    pub ollama: OllamaConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "llm-providers"))]
    pub openwebui: OpenWebuiConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "llm-providers"))]
    pub hermes: UsbHermesConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "llm-providers"))]
    pub llamacpp: UsbLlamaCppConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "infra"))]
    pub memvault: MemvaultConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "ops"))]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub server: DaemonServerConfig,
    #[serde(default)]
    #[schemars(extend("x-category" = "infra"))]
    pub relay: RelayConfig,
    #[schemars(extend("x-category" = "custom", "x-array-entry-label" = "name"))]
    #[serde(default, rename = "custom-service")]
    pub custom_services: Vec<custom_service::CustomServiceConfig>,
}

impl UsbConfig {
    /// Parse and validate a TOML string as a USB config.
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(toml_str).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    /// Parse and validate a JSON value as a USB config. Used when syncing the
    /// (full) cluster config from the management server — extra sections are
    /// ignored, only the subset is kept.
    pub fn from_json(json: &serde_json::Value) -> Result<Self, String> {
        let config: Self = serde_json::from_value(json.clone()).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.daemon.validate().map_err(|e| e.to_string())?;
        self.global.validate().map_err(|e| e.to_string())?;
        if self.ollama.enabled {
            self.ollama.validate().map_err(|e| e.to_string())?;
        }
        if self.openwebui.enabled {
            self.openwebui.validate().map_err(|e| e.to_string())?;
        }
        if self.hermes.enabled {
            self.hermes.validate().map_err(|e| e.to_string())?;
        }
        if self.llamacpp.enabled {
            self.llamacpp.validate().map_err(|e| e.to_string())?;
        }
        self.relay.validate().map_err(|e| e.to_string())?;
        let mut seen_names = std::collections::HashSet::new();
        for cs in &self.custom_services {
            cs.validate().map_err(|e| e.to_string())?;
            if !seen_names.insert(&cs.name) {
                return Err(format!("custom-service: duplicate name '{}'", cs.name));
            }
        }
        Ok(())
    }

    /// Resolve `env:` and `secret:` references in all Secret fields (server
    /// token, relay PSK, open-webui secret key).
    pub fn resolve_secrets(
        &mut self,
        env_vars: &std::collections::HashMap<String, String>,
        vault: &std::collections::HashMap<String, String>,
    ) -> Result<(), Vec<String>> {
        let mut errors = vec![];
        if let Some(ref mut token) = self.server.token {
            if let Err(e) = token.resolve(env_vars, vault) {
                errors.push(format!("server.token: {e}"));
            }
        }
        if let Some(ref mut psk) = self.relay.cluster_psk {
            if let Err(e) = psk.resolve(env_vars, vault) {
                errors.push(format!("relay.cluster_psk: {e}"));
            }
        }
        if let Some(ref mut key) = self.openwebui.secret_key {
            if let Err(e) = key.resolve(env_vars, vault) {
                errors.push(format!("openwebui.secret_key: {e}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[cfg(test)]
mod usb_config_tests {
    use super::*;
    use mac_mgmt_common::{ClusterConfig, LlmProvider};

    #[test]
    fn usb_minimal_defaults() {
        let cfg = UsbConfig::from_toml("").unwrap();
        assert!(!cfg.ollama.enabled);
        assert!(!cfg.openwebui.enabled);
        assert!(!cfg.memvault.enabled);
        assert_eq!(cfg.openwebui.port, 8088);
        assert_eq!(cfg.openwebui.host, "127.0.0.1");
    }

    #[test]
    fn usb_parses_subset_toml() {
        let toml = r#"
[ollama]
enabled = true
models = ["qwen3.5"]
default_model = "qwen3.5"

[openwebui]
enabled = true
port = 9999
auth = true

[server]
url = "https://mgmt.example.com"
token = "secret-token"
"#;
        let cfg = UsbConfig::from_toml(toml).unwrap();
        assert!(cfg.ollama.enabled);
        assert_eq!(cfg.openwebui.port, 9999);
        assert!(cfg.openwebui.auth);
        assert_eq!(cfg.server.url.as_deref(), Some("https://mgmt.example.com"));
    }

    /// A FULL cluster config (with sections the USB daemon doesn't support)
    /// must deserialize into UsbConfig, keeping the subset and silently
    /// dropping openclaw/hermes/lms/cloud/etc. This is the "honour only the
    /// subset" guarantee for config-server sync.
    #[test]
    fn usb_keeps_only_subset_from_full_cluster_json() {
        let full = serde_json::json!({
            "ollama": { "enabled": true, "models": ["qwen3.5"], "default_model": "qwen3.5" },
            "openwebui": { "enabled": true, "port": 8181 },
            "memvault": { "enabled": true },
            "openclaw": { "enabled": true },
            "hermes": { "enabled": true },
            "lms": { "enabled": true },
            "cloud": [{ "enabled": true, "provider": "anthropic" }],
            "ai_proxy": { "enabled": true },
            "global": { "default_llm": "ollama" },
        });
        let cfg = UsbConfig::from_json(&full).unwrap();
        assert!(cfg.ollama.enabled);
        assert_eq!(cfg.openwebui.port, 8181);
        assert!(cfg.memvault.enabled);
        assert_eq!(cfg.global.default_llm, LlmProvider::Ollama);
        // openclaw/hermes/lms/cloud are not fields of UsbConfig — dropped.
        let reserialized = serde_json::to_value(&cfg).unwrap();
        assert!(reserialized.get("openclaw").is_none());
        assert!(reserialized.get("lms").is_none());
        assert!(reserialized.get("cloud").is_none());
    }

    #[test]
    fn usb_full_cluster_config_has_openwebui() {
        // OpenWebui is also exposed on ClusterConfig so the server can manage it.
        let cfg = ClusterConfig::from_toml("[openwebui]\nenabled = true\nport = 7000\n").unwrap();
        assert!(cfg.openwebui.enabled);
        assert_eq!(cfg.openwebui.port, 7000);
    }

    #[test]
    fn usb_rejects_zero_openwebui_port() {
        let err = UsbConfig::from_toml("[openwebui]\nenabled = true\nport = 0\n").unwrap_err();
        assert!(err.contains("openwebui.port"), "got: {err}");
    }
}

