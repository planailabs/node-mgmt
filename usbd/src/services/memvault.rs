//! Memvault health surface for the USB daemon.
//!
//! Memvault itself runs **in-process** (its store + web app are started in
//! `run_stack` via `MemvaultHandle::init`, like `usb.rs::maybe_serve_memvault`),
//! so this is an *integrated* service (no spawned process). It exists only to
//! report memvault liveness in heartbeats and expose its web port as a tunnel.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use mac_mgmt_agent::managed_service::{ManagedService, ServiceMode, SpawnSpec, TunnelDef};

pub struct UsbMemvaultService {
    host: String,
    port: u16,
}

impl UsbMemvaultService {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

impl ManagedService for UsbMemvaultService {
    fn name(&self) -> &str {
        "memvault"
    }

    fn service_mode(&self) -> ServiceMode {
        ServiceMode::Integrated
    }

    fn ensure_installed(&self) -> Result<()> {
        Ok(())
    }
    fn ensure_setup(&self) -> Result<()> {
        Ok(())
    }
    fn repair(&self) -> Result<()> {
        Ok(())
    }
    fn check_and_upgrade(&self) -> Result<bool> {
        Ok(false)
    }

    fn spawn_spec(&self) -> SpawnSpec {
        unreachable!("spawn_spec called on integrated memvault service")
    }

    fn check_health(&self) -> Result<bool> {
        use std::net::{TcpStream, ToSocketAddrs};
        let addr = format!("{}:{}", self.host, self.port);
        let Some(sa) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            return Ok(false);
        };
        Ok(TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(2)).is_ok())
    }

    fn check_health_async(&self) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            let url = format!("http://{}:{}/", self.host, self.port);
            let client = reqwest::Client::new();
            match tokio::time::timeout(std::time::Duration::from_secs(5), client.get(&url).send())
                .await
            {
                Ok(Ok(resp)) => Ok(resp.status().is_success() || resp.status().is_redirection()),
                _ => Ok(false),
            }
        })
    }

    fn expose_tunnels(&self) -> Vec<TunnelDef> {
        vec![TunnelDef {
            name: "memvault".into(),
            host: self.host.clone(),
            tcp_port: self.port,
        }]
    }
}
