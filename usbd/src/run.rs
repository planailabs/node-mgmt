//! USB-daemon stack bring-up + phone-home event loop.
//!
//! Combines `usb.rs::run_stack` (in-process supervisor + spawn-from-mount
//! services + loopback control API) with the phone-home machinery from
//! `daemon.rs::run` (signed heartbeat, assessment probes, relay + relay-ssh,
//! server push, optional config sync), driven by the reduced [`UsbConfig`].

use std::sync::Arc;

use anyhow::{Context, Result};
use mac_mgmt_common::DaemonConfig;
use crate::usb_config::UsbConfig;
use tokio::sync::{RwLock, mpsc, watch};
use tokio::time;

use super::control::{self, ConnectionStatus, ControlState, DaemonInfo, LoopMsg};
use super::services::{self, ResolvedPorts, Resources};
use mac_mgmt_agent::assessment::{self, Assessor};
use mac_mgmt_agent::russh;
use mac_mgmt_agent::libp2p;

/// Options for bringing up the USB daemon.
#[derive(Clone, Debug)]
pub struct UsbdOpts {
    pub home: std::path::PathBuf,
    pub config_path: Option<std::path::PathBuf>,
    pub offline: bool,
    /// Fixed loopback control port (the launcher assigns one); 0 = ephemeral.
    pub control_port: u16,
}

/// Handle to a running daemon: lets the caller await + trigger shutdown.
pub struct StackHandle {
    pub control_port: u16,
    shutdown_tx: watch::Sender<bool>,
}

impl StackHandle {
    pub async fn wait_for_shutdown(&self) {
        let mut rx = self.shutdown_tx.subscribe();
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::select! {
            _ = ctrl_c => {}
            _ = async {
                loop {
                    if *rx.borrow_and_update() { return; }
                    if rx.changed().await.is_err() { return; }
                }
            } => {}
        }
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Build a throwaway full DaemonConfig view for the Assessor so the typed ollama
/// probe + inventory run against the *effective* port (the assessor only probes,
/// never installs).
fn assessor_config(cfg: &UsbConfig, ports: &ResolvedPorts) -> DaemonConfig {
    let mut d = DaemonConfig {
        daemon: cfg.daemon.clone(),
        notifications: cfg.notifications.clone(),
        global: cfg.global.clone(),
        ollama: cfg.ollama.clone(),
        memvault: cfg.memvault.clone(),
        metrics: cfg.metrics.clone(),
        server: cfg.server.clone(),
        relay: cfg.relay.clone(),
        custom_services: cfg.custom_services.clone(),
        ..Default::default()
    };
    if let Some(p) = ports.ollama {
        d.ollama.port = p;
    }
    d
}

/// Map a changed config section to the affected service name(s), so a live
/// config apply restarts only what changed.
fn changed_service_names(old: &UsbConfig, new: &UsbConfig) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let ov = serde_json::to_value(old).unwrap_or_default();
    let nv = serde_json::to_value(new).unwrap_or_default();
    if ov.get("ollama") != nv.get("ollama") {
        set.insert("ollama".to_string());
    }
    if ov.get("openwebui") != nv.get("openwebui") {
        set.insert("open-webui".to_string());
    }
    if ov.get("hermes") != nv.get("hermes") {
        set.insert("hermes".to_string());
    }
    if ov.get("llamacpp") != nv.get("llamacpp") {
        set.insert("llamacpp".to_string());
    }
    set
}

fn info_snapshot(ports: &ResolvedPorts, memvault_url: &Option<String>) -> DaemonInfo {
    DaemonInfo {
        ollama_port: ports.ollama,
        webui_port: ports.openwebui,
        memvault_port: ports.memvault,
        hermes_port: ports.hermes,
        hermes_webui_port: ports.hermes_webui,
        llamacpp_port: ports.llamacpp,
        webui_url: ports.openwebui.map(|p| format!("http://127.0.0.1:{p}")),
        memvault_url: memvault_url.clone(),
        hermes_url: ports.hermes.map(|p| format!("http://127.0.0.1:{p}")),
        hermes_webui_url: ports.hermes_webui.map(|p| format!("http://127.0.0.1:{p}")),
        llamacpp_url: ports.llamacpp.map(|p| format!("http://127.0.0.1:{p}")),
    }
}

/// Bring up the full daemon and return once everything is running. The event
/// loop runs in a spawned task until shutdown.
pub async fn run_stack(opts: UsbdOpts) -> Result<StackHandle> {
    tracing::info!(
        home = %opts.home.display(),
        offline = opts.offline,
        "starting plan.ai USB daemon",
    );
    std::fs::create_dir_all(&opts.home)
        .with_context(|| format!("create home dir {}", opts.home.display()))?;

    // Load the reduced config (local + optional remote subset sync).
    let mut cfg = super::config::load(&opts.home, opts.config_path.as_deref(), opts.offline)
        .await
        .context("failed to load usb config")?;

    // Run the services supervisor in-process.
    // SAFETY: set before ServiceManager::init reads it; single consumer.
    unsafe {
        std::env::set_var("INPROCESS_SERVICE_MANAGER", "1");
    }

    let res = Resources::from_env();
    tracing::info!(?res, "resolved mount resources");

    let (mut services_vec, mut ports) = services::build_usb_services(&cfg, &res);

    let dispatcher = Arc::new(mac_mgmt_agent::notify::Dispatcher::new(
        std::mem::take(&mut cfg.notifications.urls),
        cfg.notifications.events.take(),
    ));
    let log_buf = mac_mgmt_agent::log_buffer::LogBuffer::new();
    let metrics = Arc::new(mac_mgmt_agent::metrics::Metrics::new());

    // Identity (stable instance id from the on-stick SSH host key).
    let host_key = Arc::new(
        mac_mgmt_agent::host_keys::load_or_generate().context("load/generate SSH host key")?,
    );
    let instance_id = mac_mgmt_agent::host_keys::fingerprint_hex(&host_key);
    tracing::info!("instance id: {instance_id}");

    // Memvault in-process (store + web app), like usb.rs::maybe_serve_memvault.
    let memvault_url = maybe_serve_memvault(&cfg, &host_key).await;
    if let Some(url) = &memvault_url {
        let port = if cfg.memvault.port == 0 { 8088 } else { cfg.memvault.port };
        ports.memvault = Some(port);
        services_vec.push(Arc::new(services::memvault::UsbMemvaultService::new(
            "127.0.0.1", port,
        )));
        tracing::info!("memvault web app at {url}");
    }

    // A DaemonConfig view drives the supervisor init (config_store) + assessor.
    let mut dcfg = assessor_config(&cfg, &ports);

    let mut svc_mgr = mac_mgmt_agent::service_mgmt::ServiceManager::init_with_services(
        &mut dcfg,
        services_vec,
        Arc::clone(&dispatcher),
        log_buf.clone(),
    )
    .context("init service manager")?;
    svc_mgr.register_metrics(&metrics);

    let assessor = Arc::new(Assessor::new());
    assessor.update_config(assessor_config(&cfg, &ports)).await;
    assessor.attach_metrics(Arc::clone(&metrics)).await;

    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let (loop_tx, loop_rx) = mpsc::channel::<LoopMsg>(8);
    let info = Arc::new(RwLock::new(info_snapshot(&ports, &memvault_url)));
    let connection = Arc::new(RwLock::new(ConnectionStatus::default()));

    // Loopback control + status server.
    let control_state = ControlState {
        socket_path: mac_mgmt_services::default_socket_path(),
        offline: opts.offline,
        loop_tx,
        shutdown_tx: shutdown_tx.clone(),
        config_read: super::config::config_read_candidates(&opts.home, opts.config_path.as_deref()),
        config_write: super::config::config_write_path(&opts.home),
        info: Arc::clone(&info),
        connection: Arc::clone(&connection),
    };
    let listener = tokio::net::TcpListener::bind(("::1", opts.control_port))
        .await
        .context("bind control server")?;
    let control_port = listener.local_addr()?.port();
    let router = control::router(control_state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!("control server exited: {e}");
        }
    });
    // Write the chosen port so the launcher can discover it.
    let _ = std::fs::write(opts.home.join(".usbd-control-port"), control_port.to_string());
    tracing::info!("usb daemon control/status API on http://[::1]:{control_port}");

    // Relay + relay-ssh + p2p swarm + heartbeat/sync are the "network parts",
    // gated behind the runtime `USBD_NETWORKED` flag (the launcher exports it
    // when the drive's `mgmt` feature is on). With it OFF (default) we run a
    // purely local stack: no server (→ heartbeat/probe-upload/sync all no-op),
    // no relay registration, no p2p swarm, no server push. relay_mgr is still
    // built (its FIFO watcher is local), but without the p2p swarm it never
    // reaches a relay.
    let future = super::networked();
    let server_url = if future { cfg.server.url.clone() } else { None };
    let server_token = if future {
        cfg.server.token.as_ref().map(|s| s.expose().to_string())
    } else {
        None
    };

    let (relay_mgr, relay_heartbeat_rx) = mac_mgmt_agent::remote_ssh::RemoteSshState::new(
        server_url.clone(),
        server_token.clone(),
        future && cfg.relay.remote_ssh_enabled,
    );

    let p2p_mgr = if future
        && !opts.offline
        && (cfg.relay.relay_multiaddr.is_some() || cfg.relay.mdns_enabled || memvault_url.is_some())
    {
        build_p2p(&cfg, &host_key, &instance_id, control_port, &relay_mgr, &server_token).await
    } else {
        None
    };

    // Server push (SSE) — gated behind `future`.
    let push_rx = if let (true, Some(url), Some(token)) =
        (future && !opts.offline, server_url.as_deref(), server_token.as_deref())
    {
        let (_h, rx) = mac_mgmt_agent::server_push::start(url, token);
        Some(rx)
    } else {
        None
    };

    relay_mgr.sync_ssh_keys();
    // Register all tunnel kinds with the relay manager up front (parity with the
    // upstream mac-mgmt daemon) — regular + overrides + file + shell (the last also
    // registers the virtual service-restart/restart-daemon handlers). Refreshed later
    // via LoopState::update_relay_tunnels on health tick / install / config apply.
    relay_mgr.update_tunnel_defs(svc_mgr.collect_tunnels());
    relay_mgr.update_tunnel_overrides(svc_mgr.collect_tunnel_overrides());
    relay_mgr.update_file_tunnel_defs(svc_mgr.collect_file_tunnels());
    relay_mgr.update_shell_tunnel_defs(svc_mgr.collect_shell_tunnels());

    // Kick the initial install + start.
    svc_mgr.retry_failed_installs();
    svc_mgr.schedule_restart().await;

    let loop_state = LoopState {
        home: opts.home.clone(),
        config_path: opts.config_path.clone(),
        cfg,
        dcfg,
        res,
        ports,
        memvault_url,
        svc_mgr,
        assessor,
        metrics,
        host_key,
        instance_id,
        server_url,
        server_token,
        relay_mgr,
        p2p_mgr,
        info,
        connection,
        offline: opts.offline,
    };

    tokio::spawn(event_loop(
        loop_state,
        loop_rx,
        relay_heartbeat_rx,
        push_rx,
        shutdown_tx.clone(),
    ));

    Ok(StackHandle {
        control_port,
        shutdown_tx,
    })
}

/// Long-lived state owned by the event loop task.
struct LoopState {
    home: std::path::PathBuf,
    config_path: Option<std::path::PathBuf>,
    cfg: UsbConfig,
    /// Cached DaemonConfig view (kept in sync for the assessor).
    dcfg: DaemonConfig,
    res: Resources,
    ports: ResolvedPorts,
    memvault_url: Option<String>,
    svc_mgr: mac_mgmt_agent::service_mgmt::ServiceManager,
    assessor: Arc<Assessor>,
    metrics: Arc<mac_mgmt_agent::metrics::Metrics>,
    host_key: Arc<russh::keys::PrivateKey>,
    instance_id: String,
    server_url: Option<String>,
    server_token: Option<String>,
    relay_mgr: mac_mgmt_agent::remote_ssh::RemoteSshState,
    p2p_mgr: Option<mac_mgmt_agent::p2p::P2pManager>,
    info: Arc<RwLock<DaemonInfo>>,
    connection: Arc<RwLock<ConnectionStatus>>,
    offline: bool,
}

impl LoopState {
    fn relay_proxy_url(&self) -> Option<String> {
        self.p2p_mgr.as_ref().and_then(|m| m.relay_proxy_url())
    }

    /// Refresh the connection snapshot served at `/connection` from live state:
    /// heartbeat counters/timestamp (prometheus gauges), relay registration, and
    /// whether a server is configured at all.
    async fn update_connection(&self) {
        let last = self.metrics.heartbeat_last_success.get();
        let relay_connected = self
            .p2p_mgr
            .as_ref()
            .map(|m| m.relay_registered().load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false);
        let snap = ConnectionStatus {
            networked: !self.offline && super::networked(),
            remote_configured: self.server_url.is_some(),
            heartbeat_last_success_unix: (last > 0).then_some(last as u64),
            heartbeat_success: self.metrics.heartbeat_total.with_label_values(&["success"]).get() as u64,
            heartbeat_failure: self.metrics.heartbeat_total.with_label_values(&["failure"]).get() as u64,
            relay_enabled: self.p2p_mgr.is_some(),
            relay_connected,
        };
        *self.connection.write().await = snap;
    }

    async fn send_heartbeat(&self) {
        let (Some(url), Some(token)) = (&self.server_url, &self.server_token) else {
            return;
        };
        let services = self.svc_mgr.collect_statuses();
        let tunnels: Vec<serde_json::Value> = if self.cfg.relay.tunnels_enabled {
            self.svc_mgr
                .collect_tunnels()
                .iter()
                .map(|t| serde_json::json!({ "name": t.name, "port": t.tcp_port }))
                .collect()
        } else {
            vec![]
        };
        // File + shell tunnels announced alongside regular tunnels (parity with the
        // upstream mac-mgmt daemon's heartbeat); shapes mirror its hand-built payload.
        let file_tunnels: Vec<serde_json::Value> = if self.cfg.relay.tunnels_enabled {
            self.svc_mgr
                .collect_file_tunnels()
                .iter()
                .map(|ft| {
                    let mut val = serde_json::json!({
                        "name": ft.name(), "service": ft.service, "path": ft.path(),
                        "writable": ft.writable(), "description": ft.description(),
                    });
                    let obj = val.as_object_mut().unwrap();
                    if let mac_mgmt_agent::managed_service::FileTunnelDef::Folder { include, .. } = &ft.def {
                        obj.insert("kind".into(), "directory".into());
                        obj.insert("include".into(), serde_json::to_value(include).unwrap_or(serde_json::Value::Null));
                    } else {
                        obj.insert("kind".into(), "file".into());
                    }
                    val
                })
                .collect()
        } else {
            vec![]
        };
        let shell_tunnels: Vec<serde_json::Value> = if self.cfg.relay.tunnels_enabled {
            self.svc_mgr
                .collect_shell_tunnels()
                .iter()
                .map(|st| {
                    serde_json::json!({
                        "name": st.def.name, "service": st.service, "description": st.def.description,
                        "requires_arg": st.def.arg_template.is_some(),
                        "arg_label": st.def.arg_template.as_ref().map(|t| &t.label),
                        "arg_placeholder": st.def.arg_template.as_ref().map(|t| &t.placeholder),
                    })
                })
                .collect()
        } else {
            vec![]
        };
        let sample = self.assessor.latest_sample_snapshot();
        let services_extended = self.assessor.latest_probes_snapshot();
        let service_samples = self.svc_mgr.collect_service_samples().await;
        self.metrics
            .assessment
            .update_service_samples(&service_samples);

        let identity = mac_mgmt_agent::heartbeat::HeartbeatIdentity {
            version: env!("CARGO_PKG_VERSION").to_string(),
            environment: option_env!("ENVIRONMENT").unwrap_or("dev").to_string(),
            git_sha: env!("GIT_SHA").to_string(),
        };
        let ok = mac_mgmt_agent::heartbeat::do_send_heartbeat(
            &identity,
            url,
            token,
            &self.instance_id,
            &self.host_key,
            services,
            tunnels,
            file_tunnels,
            shell_tunnels,
            None,
            self.relay_proxy_url(),
            sample,
            services_extended,
            service_samples,
        )
        .await;
        if ok {
            self.metrics.record_heartbeat_success();
        } else {
            self.metrics.record_heartbeat_failure();
        }
    }

    fn run_probes(&self, kind: assessment::probes::ProbeKind) {
        let (Some(url), Some(token)) = (&self.server_url, &self.server_token) else {
            return;
        };
        let (u, t, iid) = (url.clone(), token.clone(), self.instance_id.clone());
        let hk = Arc::clone(&self.host_key);
        let assessor = Arc::clone(&self.assessor);
        tokio::spawn(async move {
            assessor
                .run_probes_filtered(&u, &t, &iid, &hk, Some(kind))
                .await;
        });
    }

    async fn send_inventory(&self) {
        let (Some(url), Some(token)) = (&self.server_url, &self.server_token) else {
            return;
        };
        let si = self.svc_mgr.collect_service_inventories().await;
        let ss = self.svc_mgr.collect_service_security().await;
        let (u, t, iid) = (url.clone(), token.clone(), self.instance_id.clone());
        let hk = Arc::clone(&self.host_key);
        let assessor = Arc::clone(&self.assessor);
        tokio::spawn(async move {
            assessor.send_inventory(&u, &t, &iid, &hk, si, ss).await;
        });
    }

    fn update_relay_tunnels(&self) {
        if self.cfg.relay.tunnels_enabled {
            self.relay_mgr.update_tunnel_defs(self.svc_mgr.collect_tunnels());
            self.relay_mgr.update_tunnel_overrides(self.svc_mgr.collect_tunnel_overrides());
            self.relay_mgr.update_file_tunnel_defs(self.svc_mgr.collect_file_tunnels());
            // also (re)registers the virtual service-restart/restart-daemon handlers.
            self.relay_mgr.update_shell_tunnel_defs(self.svc_mgr.collect_shell_tunnels());
        } else {
            self.relay_mgr.update_tunnel_defs(vec![]);
            self.relay_mgr.update_tunnel_overrides(std::collections::HashMap::new());
            self.relay_mgr.update_file_tunnel_defs(vec![]);
            self.relay_mgr.update_shell_tunnel_defs(vec![]);
        }
    }

    /// Apply an edited UsbConfig at runtime.
    async fn apply_config(&mut self, new_cfg: UsbConfig) {
        let (desired, ports) = services::build_usb_services(&new_cfg, &self.res);
        let restart_names = changed_service_names(&self.cfg, &new_cfg);
        self.svc_mgr.apply_services(desired, restart_names).await;
        self.ports.ollama = ports.ollama;
        self.ports.openwebui = ports.openwebui;
        self.dcfg = assessor_config(&new_cfg, &self.ports);
        self.assessor.update_config(self.dcfg.clone()).await;
        *self.info.write().await = info_snapshot(&self.ports, &self.memvault_url);
        self.update_relay_tunnels();
        self.cfg = new_cfg;
    }
}

async fn event_loop(
    mut st: LoopState,
    mut loop_rx: mpsc::Receiver<LoopMsg>,
    mut relay_heartbeat_rx: mpsc::Receiver<()>,
    mut push_rx: Option<mpsc::Receiver<mac_mgmt_agent::server_push::PushCommand>>,
    shutdown_tx: watch::Sender<bool>,
) {
    let health_interval = humantime::parse_duration(&st.cfg.daemon.health_interval)
        .unwrap_or_else(|_| std::time::Duration::from_secs(60));
    let mut health_tick = time::interval(health_interval);
    let mut heartbeat_tick = time::interval(health_interval);
    let mut liveness_tick =
        time::interval(assessment::jittered(assessment::DEFAULT_LIVENESS_INTERVAL, 10));
    let mut functional_tick =
        time::interval(assessment::jittered(assessment::DEFAULT_FUNCTIONAL_INTERVAL, 120));
    let mut inventory_tick = time::interval(assessment::DEFAULT_INVENTORY_INTERVAL);
    let update_interval = humantime::parse_duration(&st.cfg.daemon.update_interval)
        .unwrap_or_else(|_| std::time::Duration::from_secs(3600));
    let mut config_poll_tick = time::interval(update_interval);

    let mut sigterm = match mac_mgmt_agent::platform::ShutdownSignal::terminate() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to register SIGTERM: {e}");
            return;
        }
    };
    let mut shutdown_rx = shutdown_tx.subscribe();

    // Seed the connection snapshot before the first tick so the dashboard sees
    // the configured/networked state immediately (heartbeat counters fill in).
    st.update_connection().await;

    loop {
        tokio::select! {
            _ = sigterm.recv() => { tracing::info!("SIGTERM; shutting down"); break; }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { tracing::info!("shutdown requested"); break; }
            }

            _ = health_tick.tick() => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    st.svc_mgr.health_tick(&st.metrics, false),
                ).await;
                st.update_relay_tunnels();
                st.assessor.refresh_sample().await;
            }

            _ = heartbeat_tick.tick() => {
                st.assessor.refresh_sample().await;
                st.send_heartbeat().await;
                st.update_connection().await;
            }

            _ = liveness_tick.tick() => st.run_probes(assessment::probes::ProbeKind::Liveness),
            _ = functional_tick.tick() => st.run_probes(assessment::probes::ProbeKind::Functional),
            _ = inventory_tick.tick() => st.send_inventory().await,

            _ = config_poll_tick.tick() => {
                // Re-sync the subset config from the server (catches changes the
                // SSE push missed) and apply live.
                if !st.offline && st.server_url.is_some() {
                    let home = st.home.clone();
                    let cfgp = st.config_path.clone();
                    match super::config::load(&home, cfgp.as_deref(), st.offline).await {
                        Ok(new_cfg) => {
                            st.apply_config(new_cfg).await;
                            st.svc_mgr.health_tick(&st.metrics, false).await;
                        }
                        Err(e) => tracing::warn!("config poll reload failed: {e}"),
                    }
                }
            }

            _install = async {
                match &mut st.svc_mgr.install_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match _install {
                    Some(u) => st.svc_mgr.handle_install_update(u),
                    None => st.svc_mgr.finish_installs(),
                }
                st.update_relay_tunnels();
                st.send_heartbeat().await;
            }

            Some(msg) = loop_rx.recv() => {
                match msg {
                    LoopMsg::ApplyConfig { cfg, resp } => {
                        st.apply_config(*cfg).await;
                        st.svc_mgr.health_tick(&st.metrics, false).await;
                        let _ = resp.send(Ok(()));
                    }
                }
            }

            Some(cmd) = async {
                match &mut push_rx { Some(rx) => rx.recv().await, None => std::future::pending().await }
            } => {
                use mac_mgmt_agent::server_push::PushCommand;
                match cmd {
                    PushCommand::RequestAssessment => {
                        st.run_probes(assessment::probes::ProbeKind::Liveness);
                        st.send_inventory().await;
                    }
                    PushCommand::SyncSshKeys => st.relay_mgr.sync_ssh_keys(),
                    _ => {}
                }
            }

            Some(cmd) = async { st.relay_mgr.recv_cmd().await } => {
                st.relay_mgr.handle_cmd(cmd);
                // Relay commands can change registration/tunnel state — refresh
                // the connection snapshot now rather than waiting for the tick.
                st.update_connection().await;
            }

            Some(()) = relay_heartbeat_rx.recv() => {
                st.send_heartbeat().await;
                st.update_connection().await;
            }

            _evt = async {
                match st.p2p_mgr.as_mut() {
                    Some(m) => m.recv_event().await,
                    None => std::future::pending().await,
                }
            } => {
                if matches!(_evt, Some(mac_mgmt_agent::p2p::P2pEvent::RelayProxyUrlAcquired)) {
                    st.send_heartbeat().await;
                }
                // Any p2p event may shift relay reachability — refresh the snapshot.
                st.update_connection().await;
            }
        }
    }

    st.svc_mgr.shutdown().await;
    st.relay_mgr.cleanup();
    tracing::info!("usb daemon shutdown complete");
}

/// Build the libp2p swarm (relay registration + memvault sync), mirroring
/// `daemon.rs`'s construction.
async fn build_p2p(
    cfg: &UsbConfig,
    host_key: &Arc<russh::keys::PrivateKey>,
    instance_id: &str,
    control_port: u16,
    relay_mgr: &mac_mgmt_agent::remote_ssh::RemoteSshState,
    server_token: &Option<String>,
) -> Option<mac_mgmt_agent::p2p::P2pManager> {
    let relay_multiaddr = cfg
        .relay
        .relay_multiaddr
        .as_deref()
        .and_then(|s| s.parse::<libp2p::Multiaddr>().ok());

    let handler_state = Arc::new(mac_mgmt_agent::p2p::handler::HandlerState {
        ssh_allowed: relay_mgr.ssh_allowed.clone(),
        tunnel_defs: relay_mgr.tunnel_defs.clone(),
        tunnel_overrides: relay_mgr.tunnel_overrides.clone(),
        file_tunnel_registry: relay_mgr.file_tunnel_registry(),
        shell_tunnel_registry: relay_mgr.shell_tunnel_registry(),
        metrics_port: control_port,
        fake_origin_local: cfg.relay.fake_origin_local,
        client: reqwest::Client::new(),
        server_ssh_keys: relay_mgr.server_ssh_keys.clone(),
        relay_ssh_key: Arc::new(RwLock::new(None)),
    });

    let cluster_id = match (&cfg.server.url, server_token) {
        (Some(url), Some(token)) => mac_mgmt_agent::p2p::fetch_cluster_id(url, token).await,
        _ => None,
    };

    let p2p_config = mac_mgmt_agent::p2p::P2pConfig {
        instance_id: instance_id.to_string(),
        cluster_psk: cfg
            .relay
            .cluster_psk
            .as_ref()
            .and_then(|s| hex::decode(s.expose()).ok()),
        relay_multiaddr,
        mdns_enabled: cfg.relay.mdns_enabled,
        p2p_port: cfg.relay.p2p_port,
        ai_proxy_distribution: cfg.relay.ai_proxy_distribution,
        server_token: server_token.clone(),
        cluster_id,
        handler_state: Some(handler_state),
        swarm_listening: None,
        relay_registered: None,
        // mac-mgmt-agent is built with `memvault` on (see Cargo.toml), so the
        // field always exists here — unlike in-daemon, no cfg gate.
        memvault: None,
    };

    match mac_mgmt_agent::p2p::P2pManager::new(host_key, p2p_config).await {
        Ok(mgr) => {
            tracing::info!(peer_id = %mgr.local_peer_id, "p2p swarm started");
            Some(mgr)
        }
        Err(e) => {
            tracing::error!("failed to start p2p swarm: {e:#}");
            None
        }
    }
}

/// Start memvault in-process (store + web app on its own loopback port).
/// Mirrors `usb.rs::maybe_serve_memvault`.
async fn maybe_serve_memvault(
    cfg: &UsbConfig,
    host_key: &Arc<russh::keys::PrivateKey>,
) -> Option<String> {
    let mut mv = cfg.memvault.clone();
    if !mv.enabled {
        return None;
    }
    if mv.port == 0 {
        mv.port = 8088;
    }
    let peer_id = match mac_mgmt_agent::p2p::identity::keypair_from_russh(host_key) {
        Ok(kp) => kp.public().to_peer_id().to_bytes(),
        Err(e) => {
            tracing::error!("memvault: derive peer_id failed: {e:#}");
            return None;
        }
    };
    match mac_mgmt_agent::memvault::MemvaultHandle::init(&mv, peer_id).await {
        Ok(handle) => {
            // Detached tasks hold the store/client; keep alive for process life.
            std::mem::forget(handle);
            Some(format!("http://127.0.0.1:{}", mv.port))
        }
        Err(e) => {
            tracing::error!("memvault: start failed: {e:#}");
            None
        }
    }
}
