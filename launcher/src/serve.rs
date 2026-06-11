//! Localhost HTTP server: serves the rust-embedded Dioxus SPA + the control API the
//! SPA calls. The API routes + JSON shapes live in the shared `plan-ai-control-api`
//! crate (the `ControlApi` trait + `router`); this module is the REAL implementor
//! (the mock-server is the other), so the two can't drift. The launcher adds the
//! embedded-SPA static fallback on top of the shared /api/* router.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    http::{header, StatusCode, Uri},
    response::IntoResponse,
    Router,
};
use mac_mgmt_services::{protocol::Notification, Client};
use plan_ai_control_api::{
    router, Accel, ApiError, ControlApi, Gpu, Info, Platforms, ProxyReply, ServiceState,
    ServiceStatus, UpdateState, UpdateStatus,
};
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
    updater: crate::update::Handle,
    apply_requested: Arc<AtomicBool>,
    electron: crate::ElectronHandle,
    /// Base URL of the USB daemon's loopback control server (config endpoints
    /// proxy here). `None` when the daemon isn't running (config tab degrades
    /// gracefully). Set from `PLANAI_USBD_URL` by the launcher when it spawns
    /// the daemon.
    usbd_url: Option<String>,
}

/// Start the control server. Returns the base URL; the server runs on a task.
pub async fn run_server(
    client: Client,
    port: u16,
    spinner: crate::SpinnerHandle,
    updater: crate::update::Handle,
    apply_requested: Arc<AtomicBool>,
    electron: crate::ElectronHandle,
) -> anyhow::Result<String> {
    let (logs_tx, _) = broadcast::channel::<String>(512);
    let api = Arc::new(RealApi {
        client: Arc::new(Mutex::new(client)),
        logs: logs_tx,
        gpu_json: std::env::var("PLANAI_GPU_JSON").ok(),
        llmfit_url: std::env::var("PLANAI_LLMFIT_URL").ok(),
        webui_url: config::webui_url(),
        spinner,
        updater,
        apply_requested,
        electron,
        usbd_url: std::env::var("PLANAI_USBD_URL").ok(),
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

/// Map a supervisor status + an OS-level health probe to a [`ServiceState`].
async fn service_state(st: Option<&mac_mgmt_services::protocol::ServiceStatus>, health_url: &str) -> ServiceState {
    match st {
        None => ServiceState::Stopped,
        Some(s) if s.stopped => ServiceState::Stopped,
        Some(s) if s.pid.is_none() => ServiceState::Starting,
        Some(_) => {
            if control::http_ok(health_url).await {
                ServiceState::Ready
            } else {
                ServiceState::Starting
            }
        }
    }
}

/// The contract's canonical service id → the supervisor's internal name. The
/// `webui` → `open-webui` alias lives here and nowhere else (standards §7).
fn supervisor_name(svc: &str) -> &str {
    if svc == "webui" {
        "open-webui"
    } else {
        svc
    }
}

impl ControlApi for RealApi {
    fn info(&self) -> impl Future<Output = Info> + Send {
        let gpu_json = self.gpu_json.clone();
        let llmfit_url = self.llmfit_url.clone();
        let mut webui_url = self.webui_url.clone();
        let usbd_url = self.usbd_url.clone();
        async move {
            let mut ollama_port = config::ollama_port();
            let mut webui_port = config::webui_port();
            // In daemon mode the daemon resolves the EFFECTIVE ports (after any
            // collision fallback), so prefer what it reports — otherwise the
            // WebUI iframe could point at a port the daemon didn't actually bind.
            if let Some(base) = &usbd_url {
                if let Ok(r) = proxy::get(base, "/info").await {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&r.body) {
                        if let Some(p) = v.get("ollama_port").and_then(|x| x.as_u64()) {
                            ollama_port = p as u16;
                        }
                        if let Some(p) = v.get("webui_port").and_then(|x| x.as_u64()) {
                            webui_port = p as u16;
                        }
                        if let Some(u) = v.get("webui_url").and_then(|x| x.as_str()) {
                            webui_url = u.to_string();
                        }
                    }
                }
            }
            Info {
                webui_url,
                llmfit_url,
                ollama_port,
                webui_port,
                models_dir: paths::models_dir().to_string_lossy().into_owned(),
                data_dir: paths::data_dir().to_string_lossy().into_owned(),
                accel: Accel {
                    flavour: std::env::var("PLANAI_OLLAMA_FLAVOUR").ok(),
                    reason: std::env::var("PLANAI_OLLAMA_REASON").ok(),
                },
                gpu: gpu_json.as_deref().and_then(|j| serde_json::from_str::<Gpu>(j).ok()),
            }
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
                ServiceStatus { id: "ollama".into(), name: "Ollama".into(), state: ollama },
                ServiceStatus { id: "webui".into(), name: "Open-WebUI".into(), state: webui },
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
            let svc = supervisor_name(&svc).to_string();
            let mut c = client.lock().await;
            let names: Vec<String> = if svc == "all" {
                c.list().await.map(|l| l.into_iter().map(|x| x.name).collect()).unwrap_or_default()
            } else {
                vec![svc]
            };
            for n in &names {
                // `action` is pre-validated by the shared handler (start|stop|restart).
                let r = match action.as_str() {
                    "start" => c.start_service(n).await,
                    "stop" => c.stop_service(n).await,
                    _ => c.restart_service(n).await,
                };
                r.map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    fn update_status(&self) -> impl Future<Output = UpdateStatus> + Send {
        let s = self.updater.status();
        async move { s }
    }
    fn update_check(&self) -> impl Future<Output = ()> + Send {
        // Manual trigger: check the remote manifest + pre-download the delta.
        tokio::spawn(crate::update::check_and_predownload(self.updater.clone()));
        async {}
    }
    fn update_apply(&self) -> impl Future<Output = ()> + Send {
        // Only meaningful once Ready (a Pending is staged). Flag the apply + quit
        // Electron so main's wait returns and runs apply with the runtime down.
        if self.updater.status().state == UpdateState::Ready {
            self.apply_requested.store(true, Ordering::SeqCst);
            if let Ok(mut g) = self.electron.lock() {
                if let Some(child) = g.as_mut() {
                    let _ = child.kill();
                }
            }
        }
        async {}
    }
    fn platforms(&self) -> impl Future<Output = Platforms> + Send {
        let sel = crate::update::read_selection();
        async move {
            let available = plan_ai_manifest::KNOWN_TARGET_KEYS.iter().map(|s| s.to_string()).collect();
            let available_features =
                plan_ai_manifest::KNOWN_FEATURES.iter().map(|(n, _)| n.to_string()).collect();
            Platforms { kept: sel.platforms, available, features: sel.features, available_features }
        }
    }
    fn set_platforms(&self, kept: Vec<String>, features: Option<Vec<String>>) -> impl Future<Output = ()> + Send {
        let features = features.unwrap_or_else(|| crate::update::read_selection().features);
        crate::update::write_selection(&plan_ai_manifest::Selection::new(kept, features));
        // A changed selection usually means something to download (a re-added
        // platform, a newly enabled feature) or prune — kick a check right away so
        // saving acts on it without a separate manual "check for updates" step.
        tokio::spawn(crate::update::check_and_predownload(self.updater.clone()));
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

    fn config(&self) -> impl Future<Output = serde_json::Value> + Send {
        let base = self.usbd_url.clone();
        async move {
            match base {
                Some(b) => proxy::get(&b, "/config")
                    .await
                    .ok()
                    .and_then(|r| serde_json::from_slice(&r.body).ok())
                    .unwrap_or_else(|| serde_json::json!({})),
                None => serde_json::json!({}),
            }
        }
    }

    fn config_schema(&self) -> impl Future<Output = serde_json::Value> + Send {
        let base = self.usbd_url.clone();
        async move {
            match base {
                Some(b) => proxy::get(&b, "/config/schema")
                    .await
                    .ok()
                    .and_then(|r| serde_json::from_slice(&r.body).ok())
                    .unwrap_or_else(|| serde_json::json!({})),
                None => serde_json::json!({}),
            }
        }
    }

    fn set_config(
        &self,
        body: serde_json::Value,
    ) -> impl Future<Output = Result<serde_json::Value, String>> + Send {
        let base = self.usbd_url.clone();
        async move {
            let Some(b) = base else {
                return Err("config editing unavailable (usb daemon not running)".into());
            };
            let payload = serde_json::to_string(&body).map_err(|e| e.to_string())?;
            let r = proxy::put(&b, "/config", &payload)
                .await
                .map_err(|e| format!("usbd proxy: {e}"))?;
            let parsed: serde_json::Value =
                serde_json::from_slice(&r.body).unwrap_or_else(|_| serde_json::json!({}));
            if (200..300).contains(&r.status) {
                Ok(parsed)
            } else {
                let msg = parsed
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("usbd returned {}", r.status));
                Err(msg)
            }
        }
    }
}

/// Run a proxy call against the llmfit base, or a 503 if llmfit isn't running.
/// Error bodies are `ApiError` JSON so the `application/json` the proxy sets is
/// honest (standards §4).
async fn proxy_or_unavailable<F, Fut>(base: Option<String>, call: F) -> ProxyReply
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = anyhow::Result<proxy::ProxyResponse>>,
{
    let Some(b) = base else {
        return err_reply(503, "model browser unavailable (llmfit not running)");
    };
    match call(b).await {
        Ok(r) => ProxyReply { status: r.status, body: r.body },
        Err(e) => err_reply(502, &format!("llmfit proxy: {e}")),
    }
}

/// A `ProxyReply` carrying an `ApiError` JSON body.
fn err_reply(status: u16, message: &str) -> ProxyReply {
    let body = serde_json::to_vec(&ApiError::new(message)).unwrap_or_default();
    ProxyReply { status, body }
}
