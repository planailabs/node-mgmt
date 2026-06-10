//! All launcher HTTP goes through one shared reqwest client: the llmfit proxy
//! (serve.rs), service health checks (control.rs) and update downloads (update.rs).
//! rustls TLS with the RING provider — installed as the process default here,
//! since reqwest's `*-no-provider` feature deliberately doesn't pick one (we avoid
//! aws-lc-rs so the launcher still cross-compiles + stays static-musl).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// The shared, connection-pooled client (cheap to clone). Installs the ring
/// crypto provider as the process default on first use.
pub fn client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            // Idempotent; ignore the Err if something already installed a provider.
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .user_agent(concat!("plan-ai-launcher/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// Fetch a URL as text (e.g. the remote manifest JSON).
pub async fn get_string(url: &str) -> Result<String> {
    Ok(client().get(url).send().await?.error_for_status()?.text().await?)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Stream a URL to `dest`, verifying sha256. Downloads to `<dest>.part` (resuming
/// via HTTP Range when `resume` and a partial exists), then renames into place on a
/// verified hash. `expected_sha` empty skips verification. Same-dir rename = the
/// closest thing to atomic on the local cache fs.
///
/// `on_progress` is called as bytes land — with the cumulative bytes present for
/// THIS file (the resumed `.part` start plus everything streamed so far, capped so
/// it never exceeds the file). It's invoked per network chunk, so the UI can show
/// steady byte-level progress within a file instead of one jump per completed file.
pub async fn download_to(
    url: &str,
    dest: &Path,
    expected_sha: &str,
    _expected_size: u64,
    resume: bool,
    mut on_progress: impl FnMut(u64),
) -> Result<()> {
    let part = part_path(dest);
    if let Some(parent) = part.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut hasher = Sha256::new();
    let mut start: u64 = 0;
    if resume {
        if let Ok(existing) = tokio::fs::read(&part).await {
            hasher.update(&existing);
            start = existing.len() as u64;
        }
    } else {
        let _ = tokio::fs::remove_file(&part).await;
    }

    let mut req = client().get(url);
    if start > 0 {
        req = req.header("range", format!("bytes={start}-"));
    }
    let resp = req.send().await?.error_for_status()?;
    let resumed = resp.status().as_u16() == 206;
    if start > 0 && !resumed {
        // Server ignored Range — restart from scratch.
        hasher = Sha256::new();
        start = 0;
        let _ = tokio::fs::remove_file(&part).await;
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(start > 0)
        .truncate(start == 0)
        .open(&part)
        .await?;
    let mut got: u64 = start;
    on_progress(got); // seed the resumed baseline so the bar doesn't start at 0
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        got += chunk.len() as u64;
        on_progress(got);
    }
    file.flush().await?;
    drop(file);

    let got = hex(&hasher.finalize());
    if !expected_sha.is_empty() && got != expected_sha {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("sha mismatch for {url}: got {got}, want {expected_sha}");
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}
