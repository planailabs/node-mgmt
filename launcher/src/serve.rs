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
    router, Accel, ApiError, ConnectionStatus, ControlApi, DaemonInfo, Gpu, Info, Platforms,
    ProxyReply, ServiceState, ServiceStatus, UpdateState, UpdateStatus,
};
use tokio::sync::{broadcast, Mutex};

use crate::{config, control, paths, proxy};

/// The Dioxus SPA, embedded at compile time. The folder comes from
/// PLANAI_SPA_DIST (re-exported by build.rs): the flake passes the built
/// `spa` derivation directly; dev builds fall back to `spa/` beside the
/// crate (scripts/build-spa.sh).
#[derive(rust_embed::RustEmbed)]
#[folder = "$PLANAI_SPA_DIST/"]
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
    /// Model-download jobs (id → state). llmfit's serve API has NO download
    /// or installed endpoints (v0.9.x) — the launcher owns the downloads:
    /// `ollama pull` via the local server's API first, and when the model
    /// isn't in the ollama registry, a GGUF download for llama.cpp via the
    /// llmfit BINARY (`llmfit download --output-dir <models>/gguf`).
    downloads: Arc<std::sync::Mutex<std::collections::HashMap<String, DlJob>>>,
    dl_seq: std::sync::atomic::AtomicU64,
}

/// State of one model download, polled by the SPA via
/// `GET /api/llmfit/download/<id>/status` (status/progress_pct/message).
#[derive(Clone, Default)]
struct DlJob {
    status: String,
    progress_pct: f64,
    message: String,
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
        downloads: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        dl_seq: std::sync::atomic::AtomicU64::new(1),
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
    // Dev override (`--with-spa DIR` → PLANAI_SPA_DIR): serve the SPA
    // from disk instead of the embedded assets, so a locally-built SPA can be
    // iterated without rebuilding the launcher. Paths are sanitized (no `..`).
    let spa_dir = std::env::var_os("PLANAI_SPA_DIR").map(std::path::PathBuf::from);
    let serve = |path: &str| {
        if let Some(dir) = &spa_dir {
            if !path.split('/').any(|seg| seg == "..") {
                if let Ok(data) = std::fs::read(dir.join(path)) {
                    let mime = mime_guess::from_path(path).first_or_octet_stream();
                    return Some(
                        ([(header::CONTENT_TYPE, mime.as_ref().to_string())], data).into_response(),
                    );
                }
            }
            return None;
        }
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

/// Fetch the daemon's `/info` (resolved ports/URLs) as the shared typed DTO.
/// `None` when there's no daemon or it's unreachable — callers fall back to the
/// configured defaults.
async fn daemon_info(usbd_url: &Option<String>) -> Option<DaemonInfo> {
    let base = usbd_url.as_ref()?;
    let r = proxy::get(base, "/info").await.ok()?;
    serde_json::from_slice::<DaemonInfo>(&r.body).ok()
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
            let mut hermes_url = None;
            let mut hermes_webui_url = None;
            let mut memvault_url = None;
            // In daemon mode the daemon resolves the EFFECTIVE ports (after any
            // collision fallback), so prefer what it reports — otherwise the
            // WebUI iframe could point at a port the daemon didn't actually bind.
            if let Some(di) = daemon_info(&usbd_url).await {
                if let Some(p) = di.ollama_port {
                    ollama_port = p;
                }
                if let Some(p) = di.webui_port {
                    webui_port = p;
                }
                if let Some(u) = di.webui_url {
                    webui_url = u;
                }
                hermes_url = di.hermes_url;
                hermes_webui_url = di.hermes_webui_url;
                memvault_url = di.memvault_url;
            }
            Info {
                webui_url,
                llmfit_url,
                hermes_url,
                hermes_webui_url,
                memvault_url,
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

    fn connection(&self) -> impl Future<Output = ConnectionStatus> + Send {
        let usbd_url = self.usbd_url.clone();
        async move {
            // No daemon (purely local launcher) → genuinely local-only.
            let Some(base) = &usbd_url else { return ConnectionStatus::default() };
            // Don't silently collapse failures to "local only": a 404 (a usbd
            // component built before /connection existed) or a transport error
            // (daemon not up) is a real cause the dashboard would otherwise hide.
            match proxy::get(base, "/connection").await {
                Ok(r) if r.status == 200 => match serde_json::from_slice::<ConnectionStatus>(&r.body) {
                    Ok(c) => c,
                    Err(e) => {
                        crate::log(&format!("connection: daemon /connection body did not parse: {e}"));
                        ConnectionStatus::default()
                    }
                },
                Ok(r) => {
                    crate::log(&format!(
                        "connection: daemon /connection returned HTTP {} — rebuild/redeploy the usbd component (it predates the endpoint)",
                        r.status
                    ));
                    ConnectionStatus::default()
                }
                Err(e) => {
                    crate::log(&format!("connection: daemon /connection unreachable ({base}): {e}"));
                    ConnectionStatus::default()
                }
            }
        }
    }

    fn status(&self) -> impl Future<Output = Vec<ServiceStatus>> + Send {
        let client = self.client.clone();
        let usbd_url = self.usbd_url.clone();
        async move {
            let list = { client.lock().await.list().await.unwrap_or_default() };
            let by = |name: &str| list.iter().find(|x| x.name == name);
            // The daemon's resolved ports/URLs, fetched once and reused for the
            // per-feature health probes below (instead of re-querying /info each).
            let di = daemon_info(&usbd_url).await;
            let ollama = service_state(by("ollama"), &config::ollama_health_url()).await;
            let mut out = vec![ServiceStatus { id: "ollama".into(), name: "Ollama".into(), state: ollama }];
            // Feature-driven rows: only services this drive runs. open-webui is a
            // feature now; hermes appears once its supervisor entry exists.
            let sel = crate::update::read_selection();
            if sel.features.iter().any(|f| f == "openwebui") {
                let webui = service_state(by("open-webui"), &config::webui_health_url()).await;
                out.push(ServiceStatus { id: "webui".into(), name: "Open-WebUI".into(), state: webui });
            }
            if sel.features.iter().any(|f| f == "llamacpp") {
                let url = di.as_ref().and_then(|d| d.llamacpp_url.clone())
                    .unwrap_or_else(|| "http://127.0.0.1:8090".into());
                let lc = service_state(by("llamacpp"), &format!("{url}/health")).await;
                out.push(ServiceStatus { id: "llamacpp".into(), name: "llama.cpp".into(), state: lc });
            }
            if sel.features.iter().any(|f| f == "hermes") {
                // Health: the dashboard's /api/status on the effective port the
                // daemon reports (fall back to the default 9119).
                let url = di.as_ref().and_then(|d| d.hermes_url.clone())
                    .unwrap_or_else(|| "http://127.0.0.1:9119".into());
                let hermes = service_state(by("hermes"), &format!("{url}/api/status")).await;
                out.push(ServiceStatus { id: "hermes".into(), name: "Hermes".into(), state: hermes });

                // The hermes web UI (same feature, own component + service).
                let webui_url = di.as_ref().and_then(|d| d.hermes_webui_url.clone())
                    .unwrap_or_else(|| "http://127.0.0.1:9120".into());
                let webui = service_state(by("hermes-webui"), &webui_url).await;
                out.push(ServiceStatus { id: "hermes-webui".into(), name: "Hermes Web UI".into(), state: webui });
            }
            out
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
            let available = loader_manifest::KNOWN_TARGET_KEYS.iter().map(|s| s.to_string()).collect();
            let available_features =
                loader_manifest::KNOWN_FEATURES.iter().map(|(n, _)| n.to_string()).collect();
            Platforms { kept: sel.platforms, available, features: sel.features, available_features }
        }
    }
    fn set_platforms(&self, kept: Vec<String>, features: Option<Vec<String>>) -> impl Future<Output = ()> + Send {
        let features = features.unwrap_or_else(|| crate::update::read_selection().features);
        crate::update::write_selection(&loader_manifest::Selection::new(kept, features));
        // A changed selection usually means something to download (a re-added
        // platform, a newly enabled feature) or prune — kick a check right away so
        // saving acts on it without a separate manual "check for updates" step.
        tokio::spawn(crate::update::check_and_predownload(self.updater.clone()));
        async {}
    }

    fn llmfit_get(&self, path: String) -> impl Future<Output = ProxyReply> + Send {
        let base = self.llmfit_url.clone();
        let downloads = self.downloads.clone();
        // llmfit serve only exposes system/models endpoints — installed +
        // download status are launcher-owned (see `downloads`).
        async move {
            if path.starts_with("/api/v1/installed") {
                return installed_reply().await;
            }
            if let Some(id) =
                path.strip_prefix("/api/v1/download/").and_then(|r| r.strip_suffix("/status"))
            {
                let job = downloads.lock().unwrap().get(id).cloned();
                return match job {
                    Some(j) => ProxyReply {
                        status: 200,
                        body: serde_json::json!({
                            "status": j.status, "progress_pct": j.progress_pct, "message": j.message,
                        })
                        .to_string()
                        .into_bytes(),
                    },
                    None => err_reply(404, "unknown download id"),
                };
            }
            proxy_or_unavailable(base, |b| async move { proxy::get(&b, &path).await }).await
        }
    }
    fn llamacpp_get(&self, path: String) -> impl Future<Output = ProxyReply> + Send {
        // The router's effective base URL comes from the daemon's /info (same
        // source the status probe uses); None → llamacpp off → "unavailable".
        let usbd_url = self.usbd_url.clone();
        async move {
            let base = daemon_info(&usbd_url).await.and_then(|d| d.llamacpp_url.clone());
            proxy_or_unavailable(base, |b| async move { proxy::get(&b, &path).await }).await
        }
    }
    fn ollama_delete(&self, name: String) -> impl Future<Output = ProxyReply> + Send {
        async move {
            // ollama's DELETE /api/delete: {"model"} (0.30.x) — send "name" too for
            // back-compat with older servers. 200 on success (empty body).
            let body = serde_json::json!({ "model": name, "name": name }).to_string();
            match proxy::delete(&ollama_base(), "/api/delete", &body).await {
                Ok(r) => ProxyReply { status: r.status, body: r.body },
                Err(e) => err_reply(502, &format!("ollama delete failed: {e}")),
            }
        }
    }
    fn llamacpp_delete(&self, name: String) -> impl Future<Output = ProxyReply> + Send {
        async move {
            // The router's model id is a gguf filename stem. Only a bare name (no
            // path separators / `..`) — then remove <models>/gguf/<name>.gguf.
            if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
                return err_reply(400, "invalid model name");
            }
            let file = crate::paths::models_dir().join("gguf").join(format!("{name}.gguf"));
            match std::fs::remove_file(&file) {
                Ok(()) => ProxyReply { status: 200, body: br#"{"ok":true}"#.to_vec() },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => err_reply(404, "model not found"),
                Err(e) => err_reply(500, &format!("delete failed: {e}")),
            }
        }
    }
    fn llmfit_post(&self, path: String, body: String) -> impl Future<Output = ProxyReply> + Send {
        let base = self.llmfit_url.clone();
        let local: Option<ProxyReply> = if path.starts_with("/api/v1/download") {
            let model = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
            Some(match model {
                Some(model) => {
                    let id = format!(
                        "dl-{}",
                        self.dl_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    );
                    self.downloads.lock().unwrap().insert(
                        id.clone(),
                        DlJob { status: "starting".into(), progress_pct: 0.0, message: String::new() },
                    );
                    tokio::spawn(run_download(self.downloads.clone(), id.clone(), model));
                    ProxyReply {
                        status: 200,
                        body: serde_json::json!({ "id": id }).to_string().into_bytes(),
                    }
                }
                None => err_reply(422, "missing \"model\""),
            })
        } else {
            None
        };
        async move {
            match local {
                Some(r) => r,
                None => proxy_or_unavailable(base, |b| async move { proxy::post(&b, &path, &body).await }).await,
            }
        }
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

/// The local ollama server's base url (the RESOLVED running port).
fn ollama_base() -> String {
    format!("http://{}:{}", crate::config::OLLAMA_HOST, crate::running_ollama_port())
}

/// `GET /api/llmfit/installed` — the models the local ollama server has
/// (its `/api/tags`), as the `{"installed": [names]}` shape the SPA expects.
async fn installed_reply() -> ProxyReply {
    let url = format!("{}/api/tags", ollama_base());
    let names: Vec<String> = match proxy::get(&url, "").await {
        Ok(r) => serde_json::from_slice::<serde_json::Value>(&r.body)
            .ok()
            .and_then(|v| {
                v.get("models").and_then(|m| m.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect()
                })
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    ProxyReply {
        status: 200,
        body: serde_json::json!({ "installed": names }).to_string().into_bytes(),
    }
}

type DlMap = Arc<std::sync::Mutex<std::collections::HashMap<String, DlJob>>>;

fn dl_set(map: &DlMap, id: &str, status: &str, pct: f64, msg: &str) {
    if let Some(j) = map.lock().unwrap().get_mut(id) {
        j.status = status.into();
        j.progress_pct = pct;
        j.message = msg.into();
    }
}

/// One model download: `ollama pull` through the local server (streamed for
/// progress); when the model isn't in the ollama registry, fall back to a
/// GGUF download for llama.cpp via the llmfit binary (best-quant selection),
/// into `<models>/gguf/` where the usbd llama.cpp service finds it.
async fn run_download(map: DlMap, id: String, model: String) {
    use futures_util::StreamExt;

    dl_set(&map, &id, "downloading", 0.0, "ollama pull");
    let client = reqwest::Client::new();
    let url = format!("{}/api/pull", ollama_base());
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "model": model, "stream": true }))
        .send()
        .await;

    let mut registry_miss = false;
    match resp {
        Ok(r) if r.status().is_success() => {
            // ndjson stream: {"status":..., "total":..., "completed":...}
            let mut stream = r.bytes_stream();
            let mut buf = Vec::new();
            let mut failed: Option<String> = None;
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                buf.extend_from_slice(&chunk);
                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else { continue };
                    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                        failed = Some(err.to_string());
                        continue;
                    }
                    let st = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    let pct = match (
                        v.get("completed").and_then(|c| c.as_f64()),
                        v.get("total").and_then(|t| t.as_f64()),
                    ) {
                        (Some(c), Some(t)) if t > 0.0 => c / t * 100.0,
                        _ => 0.0,
                    };
                    if st == "success" {
                        dl_set(&map, &id, "complete", 100.0, "pulled into ollama");
                        return;
                    }
                    dl_set(&map, &id, "downloading", pct, st);
                }
            }
            match failed {
                // "file does not exist" / "pull model manifest" errors mean the
                // model isn't in the ollama registry → try the GGUF fallback.
                Some(e) => {
                    crate::log(&format!("ollama pull {model}: {e} — trying GGUF fallback"));
                    registry_miss = true;
                }
                None => {
                    // stream ended without an explicit success — assume done.
                    dl_set(&map, &id, "complete", 100.0, "pulled into ollama");
                    return;
                }
            }
        }
        Ok(r) => {
            crate::log(&format!("ollama pull {model}: HTTP {} — trying GGUF fallback", r.status()));
            registry_miss = true;
        }
        Err(e) => {
            crate::log(&format!("ollama pull {model}: {e} — trying GGUF fallback"));
            registry_miss = true;
        }
    }

    if !registry_miss {
        return;
    }
    let Some(lf) = std::env::var_os("PLANAI_LLMFIT_BIN").map(std::path::PathBuf::from) else {
        dl_set(&map, &id, "failed", 0.0, "not in the ollama registry and llmfit is unavailable");
        return;
    };
    let gguf_dir = crate::paths::models_dir().join("gguf");
    let _ = std::fs::create_dir_all(&gguf_dir);
    dl_set(&map, &id, "downloading", 0.0, "GGUF from HuggingFace (for llama.cpp)");
    let model2 = model.clone();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(lf)
            .args(["download", &model2, "--output-dir"])
            .arg(&gguf_dir)
            .arg("--json")
            .output()
    })
    .await;
    match out {
        Ok(Ok(o)) if o.status.success() => {
            dl_set(&map, &id, "complete", 100.0, "GGUF saved for llama.cpp");
        }
        Ok(Ok(o)) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let line = err.lines().last().unwrap_or("llmfit download failed");
            dl_set(&map, &id, "failed", 0.0, line);
        }
        _ => dl_set(&map, &id, "failed", 0.0, "llmfit download failed to run"),
    }
}

/// A `ProxyReply` carrying an `ApiError` JSON body.
fn err_reply(status: u16, message: &str) -> ProxyReply {
    let body = serde_json::to_vec(&ApiError::new(message)).unwrap_or_default();
    ProxyReply { status, body }
}
