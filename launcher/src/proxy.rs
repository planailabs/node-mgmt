//! Minimal HTTP/1.0 client used to proxy the llmfit model-browser API through the
//! launcher's localhost server (so the SPA, served same-origin, dodges CORS and
//! the llmfit port stays server-side). Hand-rolled to avoid pulling a full
//! http-client dep into the cross-compiled launcher — the same trick control.rs
//! uses for health checks. Talks HTTP/1.0 + `Connection: close` so reading to EOF
//! delimits the body.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A proxied response: the upstream status code and the raw body bytes.
pub struct ProxyResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

fn split_authority(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    Some((authority, path))
}

async fn request(method: &str, base: &str, path: &str, body: Option<&str>) -> anyhow::Result<ProxyResponse> {
    let (authority, base_path) = split_authority(base).ok_or_else(|| anyhow::anyhow!("bad base url"))?;
    let _ = base_path;
    let host = authority.split(':').next().unwrap_or(&authority).to_string();
    let mut stream = tokio::net::TcpStream::connect(&authority).await?;

    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).await?;
    }
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;

    // Split status line + headers from the body at the first CRLFCRLF.
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed response"))?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(502);
    let body = raw[sep + 4..].to_vec();
    Ok(ProxyResponse { status, body })
}

pub async fn get(base: &str, path: &str) -> anyhow::Result<ProxyResponse> {
    request("GET", base, path, None).await
}

pub async fn post(base: &str, path: &str, body: &str) -> anyhow::Result<ProxyResponse> {
    request("POST", base, path, Some(body)).await
}
