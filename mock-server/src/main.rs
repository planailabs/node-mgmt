//! Dev-only mock of the launcher control plane: implements the shared
//! `ControlApi` trait with cycling/random fake state, so the Dioxus SPA can be
//! developed standalone (`make ui` → dx serve + this) with no ollama/Electron.
//! The routes + JSON shapes come from `plan-ai-control-api` (same as the real
//! launcher), so the two can't drift. Port 9999 (PLANAI_MOCK_PORT to override).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plan_ai_control_api::{router, ControlApi, Platforms, ProxyReply, ServiceStatus, UpdateStatus};
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::broadcast;

struct State {
    ollama: String,
    webui: String,
    upd_state: String,
    upd_done: u64,
    upd_total: u64,
    kept: Vec<String>,
    dl: HashMap<String, f64>,
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
            ollama: "starting".into(),
            webui: "stopped".into(),
            upd_state: "idle".into(),
            upd_done: 0,
            upd_total: 100,
            kept: vec!["linux".into()],
            dl: HashMap::new(),
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
    let next = |s: &str, rng: &mut rand::rngs::ThreadRng| -> String {
        match s {
            "stopped" => "starting",
            "starting" => {
                let r: f64 = rng.gen();
                if r < 0.6 { "ready" } else if r < 0.7 { "error" } else { "starting" }
            }
            "ready" => if rng.gen::<f64>() < 0.9 { "ready" } else { "starting" },
            _ => "starting",
        }
        .to_string()
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
            m.ollama = next(&m.ollama.clone(), &mut rng);
            m.webui = next(&m.webui.clone(), &mut rng);
            match m.upd_state.as_str() {
                "checking" => {
                    m.upd_state = "downloading".into();
                    m.upd_done = 0;
                }
                "downloading" => {
                    m.upd_done = (m.upd_done + rng.gen_range(5..20)).min(m.upd_total);
                    if m.upd_done >= m.upd_total {
                        m.upd_state = "ready".into();
                    }
                }
                "applying" => {
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
    fn info(&self) -> impl Future<Output = Value> + Send {
        let kept = self.s.lock().unwrap().kept.clone();
        async move {
            json!({
                "webui_url": "http://127.0.0.1:8080",
                "llmfit_url": "http://127.0.0.1:8787",
                "ollama_port": 11434, "webui_port": 8080,
                "models_dir": "/Volumes/PLANAI/models", "data_dir": "/Volumes/PLANAI/data",
                "accel": { "flavour": "cpu", "reason": "no GPU detected (mock)" },
                "gpu": { "gpu_name": "Apple M-mock", "gpu_vram_gb": 16.0, "backend": "metal", "total_ram_gb": 32.0, "cpu_name": "mock cpu" },
                "platforms": kept,
            })
        }
    }
    fn status(&self) -> impl Future<Output = Vec<ServiceStatus>> + Send {
        let m = self.s.lock().unwrap();
        let v = vec![
            ServiceStatus { id: "ollama".into(), name: "Ollama".into(), state: m.ollama.clone() },
            ServiceStatus { id: "webui".into(), name: "Open-WebUI".into(), state: m.webui.clone() },
        ];
        async move { v }
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
            let target = if action == "stop" { "stopped" } else { "starting" };
            match svc.as_str() {
                "ollama" => m.ollama = target.into(),
                "webui" => m.webui = target.into(),
                "all" => {
                    m.ollama = target.into();
                    m.webui = target.into();
                }
                _ => {}
            }
        }
        async { Ok(()) }
    }
    fn update_status(&self) -> impl Future<Output = UpdateStatus> + Send {
        let m = self.s.lock().unwrap();
        let st = UpdateStatus {
            state: m.upd_state.clone(),
            done: m.upd_done,
            total: m.upd_total,
            version: "0.2.0".into(),
            commit: "deadbeefcafe".into(),
            message: None,
        };
        async move { st }
    }
    fn update_check(&self) -> impl Future<Output = ()> + Send {
        self.s.lock().unwrap().upd_state = "checking".into();
        async {}
    }
    fn update_apply(&self) -> impl Future<Output = ()> + Send {
        {
            let mut m = self.s.lock().unwrap();
            m.upd_state = "applying".into();
            m.upd_done = 0;
        }
        async {}
    }
    fn platforms(&self) -> impl Future<Output = Platforms> + Send {
        let kept = self.s.lock().unwrap().kept.clone();
        async move { Platforms { kept, available: vec!["linux".into(), "mac".into(), "win".into()] } }
    }
    fn set_platforms(&self, kept: Vec<String>) -> impl Future<Output = ()> + Send {
        self.s.lock().unwrap().kept = kept;
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
    fn llmfit_post(&self, _path: String, body: String) -> impl Future<Output = ProxyReply> + Send {
        let model = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from)).unwrap_or_default();
        let id = format!("job-{}", rand::thread_rng().gen::<u32>());
        self.s.lock().unwrap().dl.insert(id.clone(), 0.0);
        eprintln!("[mock] download {model} -> {id}");
        async move { ProxyReply { status: 200, body: json!({ "id": id }).to_string().into_bytes() } }
    }
}
