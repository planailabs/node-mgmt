//! Localhost HTTP server: serves the rust-embedded Dioxus SPA + the control API the
//! SPA calls. The API routes + JSON shapes live in the shared `plan-ai-control-api`
//! crate (the `ControlApi` trait + `router`); this module is the REAL implementor
//! (the mock-server is the other), so the two can't drift. The launcher adds the
//! embedded-SPA static fallback on top of the shared /api/* router.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    http::{header, StatusCode, Uri},
    response::IntoResponse,
    Router,
};
use mac_mgmt_services::{protocol::Notification, Client};
use plan_ai_control_api::{router, ControlApi, Platforms, ProxyReply, ServiceStatus, UpdateStatus};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::{config, control, paths, proxy};

/// The Dioxus SPA, embedded at compile time (built into spa/ by scripts/build-spa.sh
/// or the flake before the launcher compiles).
#[derive(rust_embed::RustEmbed)]
#[folder = "spa/"]
struct Spa;

/// The real control-plane backend: the supervised stack + the splash spinner.
/// (Update + platform state are stubbed here; Part C/D fill them in.)
struct RealApi {
    client: Arc<Mutex<Client>>,
    logs: broadcast::Sender<String>,
    gpu_json: Option<String>,
    llmfit_url: Option<String>,
    webui_url: String,
    spinner: crate::SpinnerHandle,
}

/// Start the control server. Returns the base URL; the server runs on a task.
pub async fn run_server(client: Client, port: u16, spinner: crate::SpinnerHandle) -> anyhow::Result<String> {
    let (logs_tx, _) = broadcast::channel::<String>(512);
    let api = Arc::new(RealApi {
        client: Arc::new(Mutex::new(client)),
        logs: logs_tx,
        gpu_json: std::env::var("PLANAI_GPU_JSON").ok(),
        llmfit_url: std::env::var("PLANAI_LLMFIT_URL").ok(),
        webui_url: config::webui_url(),
        spinner,
    });

    // Drain supervisor notifications → fan out to the SSE log subscribers.
    {
        let client = api.client.clone();
        let logs = api.logs.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut c = client.lock().await;
                    while let Some(n) = c.try_recv_notification() {
                        let _ = logs.send(fmt_notif(&n));
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
    }

    // Shared /api/* router + the embedded SPA as the fallback.
    let app: Router = router(api).fallback(static_handler);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let url = format!("http://127.0.0.1:{port}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(url)
}

fn fmt_notif(n: &Notification) -> String {
    match n {
        Notification::Log { name, line, .. } => format!("[{name}] {line}"),
        Notification::Crashed { name, exit_code } => format!("[{name}] crashed (exit {exit_code:?})"),
    }
}

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let p = uri.path().trim_start_matches('/');
    let p = if p.is_empty() { "index.html" } else { p };
    let serve = |path: &str| {
        Spa::get(path).map(|f| {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string())], f.data.into_owned()).into_response()
        })
    };
    serve(p)
        .or_else(|| serve("index.html")) // SPA fallback
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "not found").into_response())
}

/// Map a supervisor status + an OS-level health probe to {ready, starting, stopped}.
async fn service_state(st: Option<&mac_mgmt_services::protocol::ServiceStatus>, health_url: &str) -> &'static str {
    match st {
        None => "stopped",
        Some(s) if s.stopped => "stopped",
        Some(s) if s.pid.is_none() => "starting",
        Some(_) => {
            if control::http_ok(health_url).await {
                "ready"
            } else {
                "starting"
            }
        }
    }
}

/// The platform this launcher runs on, in manifest terms.
fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    }
}

impl ControlApi for RealApi {
    fn info(&self) -> impl Future<Output = Value> + Send {
        let gpu_json = self.gpu_json.clone();
        let llmfit_url = self.llmfit_url.clone();
        let webui_url = self.webui_url.clone();
        async move {
            json!({
                "webui_url": webui_url,
                "llmfit_url": llmfit_url,
                "ollama_port": config::ollama_port(),
                "webui_port": config::webui_port(),
                "models_dir": paths::models_dir().to_string_lossy(),
                "data_dir": paths::data_dir().to_string_lossy(),
                "accel": {
                    "flavour": std::env::var("PLANAI_OLLAMA_FLAVOUR").ok(),
                    "reason": std::env::var("PLANAI_OLLAMA_REASON").ok(),
                    "llmfit_url": llmfit_url,
                },
                "gpu": gpu_json.as_deref().and_then(|j| serde_json::from_str::<Value>(j).ok()),
            })
        }
    }

    fn status(&self) -> impl Future<Output = Vec<ServiceStatus>> + Send {
        let client = self.client.clone();
        async move {
            let list = { client.lock().await.list().await.unwrap_or_default() };
            let by = |name: &str| list.iter().find(|x| x.name == name);
            let ollama = service_state(by("ollama"), &config::ollama_health_url()).await;
            let webui = service_state(by("open-webui"), &config::webui_health_url()).await;
            vec![
                ServiceStatus { id: "ollama".into(), name: "Ollama".into(), state: ollama.into() },
                ServiceStatus { id: "webui".into(), name: "Open-WebUI".into(), state: webui.into() },
            ]
        }
    }

    fn subscribe_logs(&self) -> broadcast::Receiver<String> {
        self.logs.subscribe()
    }

    fn ready(&self) -> impl Future<Output = ()> + Send {
        crate::kill_spinner(&self.spinner);
        async {}
    }

    fn service_action(&self, svc: String, action: String) -> impl Future<Output = Result<(), String>> + Send {
        let client = self.client.clone();
        async move {
            let svc = if svc == "webui" { "open-webui".to_string() } else { svc };
            let mut c = client.lock().await;
            let names: Vec<String> = if svc == "all" {
                c.list().await.map(|l| l.into_iter().map(|x| x.name).collect()).unwrap_or_default()
            } else {
                vec![svc]
            };
            for n in &names {
                let r = match action.as_str() {
                    "start" => c.start_service(n).await,
                    "stop" => c.stop_service(n).await,
                    "restart" => c.restart_service(n).await,
                    _ => return Err("unknown action".into()),
                };
                r.map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    // --- update + platforms: stubbed here; wired in Part C/D ----------------
    fn update_status(&self) -> impl Future<Output = UpdateStatus> + Send {
        async { UpdateStatus::idle() }
    }
    fn update_check(&self) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn update_apply(&self) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn platforms(&self) -> impl Future<Output = Platforms> + Send {
        async {
            Platforms {
                kept: vec![current_platform().into()],
                available: vec!["linux".into(), "mac".into(), "win".into()],
            }
        }
    }
    fn set_platforms(&self, _kept: Vec<String>) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn llmfit_get(&self, path: String) -> impl Future<Output = ProxyReply> + Send {
        let base = self.llmfit_url.clone();
        async move { proxy_or_unavailable(base, |b| async move { proxy::get(&b, &path).await }).await }
    }
    fn llmfit_post(&self, path: String, body: String) -> impl Future<Output = ProxyReply> + Send {
        let base = self.llmfit_url.clone();
        async move { proxy_or_unavailable(base, |b| async move { proxy::post(&b, &path, &body).await }).await }
    }
}

/// Run a proxy call against the llmfit base, or a 503 if llmfit isn't running.
async fn proxy_or_unavailable<F, Fut>(base: Option<String>, call: F) -> ProxyReply
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = anyhow::Result<proxy::ProxyResponse>>,
{
    let Some(b) = base else {
        return ProxyReply { status: 503, body: b"model browser unavailable (llmfit not running)".to_vec() };
    };
    match call(b).await {
        Ok(r) => ProxyReply { status: r.status, body: r.body },
        Err(e) => ProxyReply { status: 502, body: format!("llmfit proxy: {e}").into_bytes() },
    }
}
