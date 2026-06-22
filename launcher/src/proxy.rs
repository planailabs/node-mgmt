//! Proxy the llmfit model-browser API through the launcher's localhost server (so
//! the SPA, served same-origin, dodges CORS and the llmfit port stays
//! server-side). Thin wrappers over the shared reqwest client (net::client()).

/// A proxied response: the upstream status code and the raw body bytes.
pub struct ProxyResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

async fn send(req: reqwest::RequestBuilder) -> anyhow::Result<ProxyResponse> {
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await?.to_vec();
    Ok(ProxyResponse { status, body })
}

pub async fn get(base: &str, path: &str) -> anyhow::Result<ProxyResponse> {
    send(crate::net::client().get(format!("{base}{path}"))).await
}

pub async fn post(base: &str, path: &str, body: &str) -> anyhow::Result<ProxyResponse> {
    send(
        crate::net::client()
            .post(format!("{base}{path}"))
            .header("content-type", "application/json")
            .body(body.to_string()),
    )
    .await
}

pub async fn put(base: &str, path: &str, body: &str) -> anyhow::Result<ProxyResponse> {
    send(
        crate::net::client()
            .put(format!("{base}{path}"))
            .header("content-type", "application/json")
            .body(body.to_string()),
    )
    .await
}

pub async fn delete(base: &str, path: &str, body: &str) -> anyhow::Result<ProxyResponse> {
    send(
        crate::net::client()
            .delete(format!("{base}{path}"))
            .header("content-type", "application/json")
            .body(body.to_string()),
    )
    .await
}
