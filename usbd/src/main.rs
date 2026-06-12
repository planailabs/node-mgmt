//! Sovereign-AI USB *daemon* (the `usbd` launcher component).
//!
//! Boots the reduced phone-home control plane for the plan-ai-usb-minimal
//! stack: it supervises ollama + open-webui (+ memvault) **run from the
//! mounted dmg/squashfs** (no nix install/upgrade), and adds the agent
//! machinery — signed heartbeat, assessment probes, relay + relay SSH, and
//! optional config-server sync of the reduced [`UsbConfig`] — from the
//! mac-mgmt-agent crate (vendored mac-mgmt submodule).
//!
//! It assumes the launcher has already mounted the components and exported
//! `PLANAI_RESOURCES` (+ the resolved `PLANAI_*` paths). The binary keeps the
//! historical `mac-mgmt` name and tolerates a leading `usbd` argv token: the
//! launcher spawns `<usbd>/mac-mgmt[.exe] usbd …` (usbd::resolve_bin), a
//! contract shared with the days this lived in the mac-mgmt daemon.
//!
//! `HOME` is pinned single-threaded, before the tokio runtime is built (and,
//! on macOS, so a future event loop could own the main thread).

mod config;
mod usb_config;
mod control;
mod run;
mod services;

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Whether the daemon's "network parts" — heartbeat, relay + relay-ssh, server
/// push, and remote config sync — are enabled for THIS run. A runtime flag
/// (`USBD_NETWORKED=1|true`, exported by the launcher when the drive's `mgmt`
/// feature is on), replacing the old compile-time `future` cargo feature: one
/// shipped binary, purely local by default, no phone-home until the user opts
/// in. `--offline` still wins over it (checked at the call sites).
pub fn networked() -> bool {
    std::env::var("USBD_NETWORKED").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// `mac-mgmt usbd` flags.
#[derive(clap::Parser, Debug)]
#[command(name = "mac-mgmt usbd", about = "Run the plan.ai USB daemon")]
pub struct UsbdCli {
    /// Stick/home directory (config, host key, caches). Default: `<exe dir>/home`.
    #[arg(long)]
    pub home: Option<PathBuf>,

    /// Config path. Default: `<home>/config.json` then `<home>/config.toml`.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// No network: skip remote config sync, heartbeat, relay, and updates.
    #[arg(long)]
    pub offline: bool,

    /// Fixed loopback control port (the launcher assigns one). 0 = ephemeral.
    #[arg(long, default_value_t = 0)]
    pub control_port: u16,

    /// Accepted for parity with `usb`; the daemon is always headless.
    #[arg(long)]
    pub headless: bool,
}

/// Process entry point. Strips a leading `usbd` token, pins `HOME` to the stick,
/// builds a multi-thread runtime, brings the stack up, and blocks until shutdown.
fn main() -> ! {
    let code = match run_main(std::env::args().collect()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run_main(mut args: Vec<String>) -> Result<()> {
    use clap::Parser;

    if args.get(1).map(String::as_str) == Some("usbd") {
        args.remove(1);
    }

    // `mac-mgmt memctl …`: the memvault management CLI, kept from the old
    // daemon binary so the on-stick memvault stays administrable (memctl is
    // already in the graph via mac-mgmt-agent's memvault feature).
    if args.get(1).map(String::as_str) == Some("memctl") {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cli = memctl::Cli::parse_from(&args[1..]);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        return rt.block_on(memctl::run(cli));
    }

    let cli = UsbdCli::parse_from(args);

    init_tracing();

    // Install the rustls crypto provider before any TLS client is built (the
    // updater, relay, memvault, config sync). The usbd entry bypasses
    // daemon_main::main(), which is where the daemon normally does this.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let home = match &cli.home {
        Some(p) => p.clone(),
        None => default_home_dir()?,
    };
    std::fs::create_dir_all(&home)
        .with_context(|| format!("create home dir {}", home.display()))?;

    // Pin everything to the stick: HOME drives the supervisor socket, host key,
    // and config dir. Done single-threaded, before the runtime.
    // SAFETY: single-threaded startup, before the tokio runtime.
    unsafe {
        std::env::set_var("HOME", &home);
    }

    let offline = cli.offline || env_flag("MAC_MGMT_OFFLINE");
    let opts = run::UsbdOpts {
        home,
        config_path: cli.config.clone(),
        offline,
        control_port: cli.control_port,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let handle = rt
        .block_on(run::run_stack(opts))
        .context("failed to start usb daemon")?;

    rt.block_on(handle.wait_for_shutdown());
    handle.request_shutdown();
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .try_init();
}

/// True for `1`/`true`/`yes`/`on` (case-insensitive).
fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Default stick/home: a `home/` subdir beside the running executable.
pub fn default_home_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolve current exe")?;
    let dir = exe.parent().context("exe has no parent")?.to_path_buf();
    Ok(dir.join("home"))
}
