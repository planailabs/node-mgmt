//! Dev-only mock of the launcher control plane: implements the shared
//! `ControlApi` trait with cycling/random fake state, so the Dioxus SPA can be
//! developed standalone (`make ui` → dx serve + this) with no ollama/Electron.
//! The routes + JSON shapes come from `plan-ai-control-api` (same as the real
//! launcher), so the two can't drift. Port 9999 (PLANAI_MOCK_PORT to override).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plan_ai_control_api::{
    router, Accel, ConnectionStatus, ControlApi, Gpu, Info, Platforms, ProxyReply, ServiceState,
    ServiceStatus, UpdateState, UpdateStatus,
};
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::broadcast;

struct State {
    ollama: ServiceState,
    webui: ServiceState,
    update: UpdateState,
    upd_done: u64,
    upd_total: u64,
    kept: Vec<String>,
    features: Vec<String>,
    dl: HashMap<String, f64>,
    /// Stored USB config (only the values "set"), round-tripped by the Config tab.
    config: Value,
}

struct Mock {
    s: Mutex<State>,
    logs: broadcast::Sender<String>,
}

#[tokio::main]
async fn main() {
    let (logs, _rx) = broadcast::channel::<String>(256);
    let mock = Arc::new(Mock {
        s: Mutex::new(State {
            ollama: ServiceState::Starting,
            webui: ServiceState::Stopped,
            update: UpdateState::Idle,
            upd_done: 0,
            upd_total: 100,
            kept: vec!["linux-x64".into()],
            features: vec!["openwebui".into()],
            dl: HashMap::new(),
            config: json!({
                "ollama": { "enabled": true, "default_model": "qwen3.5", "models": ["qwen3.5"] },
                "openwebui": { "enabled": true, "port": 8088 },
                "memvault": { "enabled": false },
            }),
        }),
        logs,
    });
    tokio::spawn(ticker(mock.clone()));

    let port: u16 = std::env::var("PLANAI_MOCK_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(9999);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    eprintln!("[mock] plan.ai mock API on http://127.0.0.1:{port}/api  (dx proxy target)");
    axum::serve(listener, router(mock)).await.unwrap();
}

/// Random-walk services, advance an in-flight update + downloads, emit fake logs.
async fn ticker(mock: Arc<Mock>) {
    let next = |st: ServiceState, rng: &mut rand::rngs::ThreadRng| -> ServiceState {
        match st {
            ServiceState::Stopped => ServiceState::Starting,
            ServiceState::Starting => {
                let r: f64 = rng.gen();
                if r < 0.6 {
                    ServiceState::Ready
                } else if r < 0.7 {
                    ServiceState::Error
                } else {
                    ServiceState::Starting
                }
            }
            ServiceState::Ready => {
                if rng.gen::<f64>() < 0.9 {
                    ServiceState::Ready
                } else {
                    ServiceState::Starting
                }
            }
            ServiceState::Error => ServiceState::Starting,
        }
    };
    let lines = [
        "[ollama] llama runner started",
        "[open-webui] Uvicorn running on 127.0.0.1:8080",
        "[ollama] POST /api/chat 200",
        "[open-webui] GET /health 200",
        "[supervisor] heartbeat ok",
    ];
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let line = {
            let mut rng = rand::thread_rng();
            let mut m = mock.s.lock().unwrap();
            m.ollama = next(m.ollama, &mut rng);
            m.webui = next(m.webui, &mut rng);
            match m.update {
                UpdateState::Checking => {
                    m.update = UpdateState::Downloading;
                    m.upd_done = 0;
                }
                UpdateState::Downloading => {
                    m.upd_done = (m.upd_done + rng.gen_range(5..20)).min(m.upd_total);
                    if m.upd_done >= m.upd_total {
                        m.update = UpdateState::Ready;
                    }
                }
                UpdateState::Applying => {
                    m.upd_done = (m.upd_done + rng.gen_range(8..25)).min(m.upd_total);
                }
                _ => {}
            }
            for v in m.dl.values_mut() {
                if *v < 100.0 {
                    *v = (*v + rng.gen_range(7.0..22.0)).min(100.0);
                }
            }
            lines[rng.gen_range(0..lines.len())].to_string()
        };
        let _ = mock.logs.send(line);
    }
}

impl ControlApi for Mock {
    fn info(&self) -> impl Future<Output = Info> + Send {
        async move {
            Info {
                hermes_url: Some("http://127.0.0.1:9119".into()),
                hermes_webui_url: Some("http://127.0.0.1:9120".into()),
                memvault_url: Some("http://127.0.0.1:8088".into()),
                webui_url: "http://127.0.0.1:8080".into(),
                llmfit_url: Some("http://127.0.0.1:11436".into()),
                ollama_port: 11434,
                webui_port: 8080,
                models_dir: "/Volumes/PLANAI/models".into(),
                data_dir: "/Volumes/PLANAI/data".into(),
                accel: Accel { flavour: Some("cpu".into()), reason: Some("no GPU detected (mock)".into()) },
                gpu: Some(Gpu {
                    gpu_name: Some("Apple M-mock".into()),
                    gpu_vram_gb: Some(16.0),
                    backend: Some("metal".into()),
                    total_ram_gb: Some(32.0),
                    cpu_name: Some("mock cpu".into()),
                }),
            }
        }
    }
    fn status(&self) -> impl Future<Output = Vec<ServiceStatus>> + Send {
        let m = self.s.lock().unwrap();
        let mut v = vec![
            ServiceStatus { id: "ollama".into(), name: "Ollama".into(), state: m.ollama },
            ServiceStatus { id: "webui".into(), name: "Open-WebUI".into(), state: m.webui },
        ];
        // hermes ships two services (dashboard + web UI) under one feature; mark
        // them ready when the feature is on so the app switcher is previewable.
        if m.features.iter().any(|f| f == "hermes") {
            v.push(ServiceStatus { id: "hermes".into(), name: "Hermes".into(), state: ServiceState::Ready });
            v.push(ServiceStatus { id: "hermes-webui".into(), name: "Hermes Web UI".into(), state: ServiceState::Ready });
        }
        async move { v }
    }
    fn connection(&self) -> impl Future<Output = ConnectionStatus> + Send {
        // The "mgmt" feature is what turns on the networked/remote parts, so
        // gate the (fake) connected state on it — toggle it on the dashboard to
        // preview the connection card live.
        let mgmt = self.s.lock().unwrap().features.iter().any(|f| f == "mgmt");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let snap = ConnectionStatus {
            networked: mgmt,
            remote_configured: mgmt,
            heartbeat_last_success_unix: mgmt.then_some(now.saturating_sub(12)),
            heartbeat_success: if mgmt { 142 } else { 0 },
            heartbeat_failure: if mgmt { 3 } else { 0 },
            relay_enabled: mgmt,
            relay_connected: mgmt,
        };
        async move { snap }
    }
    fn subscribe_logs(&self) -> broadcast::Receiver<String> {
        self.logs.subscribe()
    }
    fn ready(&self) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn service_action(&self, svc: String, action: String) -> impl Future<Output = Result<(), String>> + Send {
        {
            let mut m = self.s.lock().unwrap();
            let target = if action == "stop" { ServiceState::Stopped } else { ServiceState::Starting };
            match svc.as_str() {
                "ollama" => m.ollama = target,
                "webui" => m.webui = target,
                "all" => {
                    m.ollama = target;
                    m.webui = target;
                }
                _ => {}
            }
        }
        async { Ok(()) }
    }
    fn update_status(&self) -> impl Future<Output = UpdateStatus> + Send {
        let m = self.s.lock().unwrap();
        // Fake bytes/throughput (~32 MiB/file, ~12 MiB/s) so the dev SPA shows the
        // throughput indicator during downloading/applying.
        let active = matches!(m.update, UpdateState::Downloading | UpdateState::Applying);
        let st = UpdateStatus {
            state: m.update,
            done: m.upd_done,
            total: m.upd_total,
            done_bytes: m.upd_done.saturating_mul(33_500_000),
            total_bytes: m.upd_total.saturating_mul(33_500_000),
            rate_bps: if active { 12_500_000 } else { 0 },
            version: "0.2.0".into(),
            commit: "deadbeefcafe".into(),
            message: None,
        };
        async move { st }
    }
    fn update_check(&self) -> impl Future<Output = ()> + Send {
        self.s.lock().unwrap().update = UpdateState::Checking;
        async {}
    }
    fn update_apply(&self) -> impl Future<Output = ()> + Send {
        {
            let mut m = self.s.lock().unwrap();
            m.update = UpdateState::Applying;
            m.upd_done = 0;
        }
        async {}
    }
    fn platforms(&self) -> impl Future<Output = Platforms> + Send {
        let (kept, features) = {
            let m = self.s.lock().unwrap();
            (m.kept.clone(), m.features.clone())
        };
        async move {
            Platforms {
                kept,
                available: vec!["linux-x64".into(), "linux-arm64".into(), "win-x64".into(), "mac-arm64".into()],
                features,
                available_features: vec!["openwebui".into(), "hermes".into(), "mgmt".into()],
            }
        }
    }
    fn set_platforms(&self, kept: Vec<String>, features: Option<Vec<String>>) -> impl Future<Output = ()> + Send {
        let mut m = self.s.lock().unwrap();
        m.kept = kept;
        if let Some(f) = features {
            m.features = f;
        }
        // mirror the real backend: saving a selection kicks an update check
        m.update = UpdateState::Checking;
        drop(m);
        async {}
    }
    fn llmfit_get(&self, path: String) -> impl Future<Output = ProxyReply> + Send {
        let body = if path.contains("/installed") {
            json!({ "installed": ["llama3.2:3b", "qwen2.5:7b"] })
        } else if path.contains("/download/") {
            let pct = self.s.lock().unwrap().dl.values().copied().fold(100.0_f64, |_, v| v);
            let status = if pct >= 100.0 { "complete" } else { "downloading" };
            json!({ "status": status, "progress_pct": pct, "message": format!("{pct:.0}%") })
        } else {
            let models: Vec<Value> = ["llama3.2:3b", "qwen2.5:7b", "phi4:14b", "gemma2:9b", "mistral:7b"]
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let level = ["perfect", "good", "marginal"][i % 3];
                    let label = ["perfect", "good", "marginal+"][i % 3];
                    let params = [3, 7, 14, 9, 7][i];
                    let mode = if i % 2 == 0 { "GPU" } else { "CPU" };
                    json!({
                        "name": n,
                        "fit_level": level,
                        "fit_label": label,
                        "parameter_count": format!("{params}B"),
                        "best_quant": "Q4_K_M",
                        "run_mode_label": mode,
                        "estimated_tps": (60 - i * 8) as f64,
                    })
                })
                .collect();
            json!({
                "system": { "gpu_name": "Apple M-mock", "gpu_vram_gb": 16.0, "backend": "metal", "total_ram_gb": 32.0, "cpu_name": "mock cpu" },
                "returned_models": models.len(), "total_models": 42, "models": models,
            })
        };
        async move { ProxyReply { status: 200, body: body.to_string().into_bytes() } }
    }
    fn llamacpp_get(&self, _path: String) -> impl Future<Output = ProxyReply> + Send {
        // OpenAI /v1/models shape (router lists by filename stem).
        let data: Vec<Value> = ["qwen2.5-7b-instruct-q4_k_m", "llama-3.2-3b-instruct-q4_k_m"]
            .iter()
            .map(|id| json!({ "id": id, "object": "model", "owned_by": "llamacpp" }))
            .collect();
        let body = json!({ "object": "list", "data": data });
        async move { ProxyReply { status: 200, body: body.to_string().into_bytes() } }
    }
    fn llmfit_post(&self, _path: String, body: String) -> impl Future<Output = ProxyReply> + Send {
        let model = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from)).unwrap_or_default();
        let id = format!("job-{}", rand::thread_rng().gen::<u32>());
        self.s.lock().unwrap().dl.insert(id.clone(), 0.0);
        eprintln!("[mock] download {model} -> {id}");
        async move { ProxyReply { status: 200, body: json!({ "id": id }).to_string().into_bytes() } }
    }

    fn config(&self) -> impl Future<Output = Value> + Send {
        let v = self.s.lock().unwrap().config.clone();
        async move { v }
    }
    fn config_schema(&self) -> impl Future<Output = Value> + Send {
        // The REAL reduced subset schema, so the editor matches the daemon.
        let schema = schemars::schema_for!(plan_ai_usb_config::UsbConfig);
        let v = serde_json::to_value(&schema).unwrap_or_else(|_| json!({}));
        async move { v }
    }
    fn set_config(&self, body: Value) -> impl Future<Output = Result<Value, String>> + Send {
        // Validate against UsbConfig (mirrors the daemon's PUT), then store.
        let result = match plan_ai_usb_config::UsbConfig::from_json(&body) {
            Ok(_) => {
                self.s.lock().unwrap().config = body.clone();
                eprintln!("[mock] config saved");
                Ok(json!({ "ok": true, "applied": true, "note": "stored (mock)" }))
            }
            Err(e) => Err(format!("invalid config: {e}")),
        };
        async move { result }
    }
}
