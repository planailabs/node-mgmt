//! The loader build orchestrator. The consumer's Makefile is the user entrypoint; it
//! shells to this tool (built offline by nix: `nix run .#xtask`). Everything it knows
//! about the product is DATA read from `loader.toml` (via `loader-engine`): targets,
//! flavour catalogs, features, components, classification, drive layout, update
//! identity. The tool itself is project-agnostic.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use loader_engine::Config;
use loader_manifest::ClassifyTable;

#[derive(Parser)]
#[command(name = "xtask", about = "loader build orchestrator (loader.toml-driven)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate an update manifest (JSON) for a drive-layout root directory.
    GenManifest(ManifestArgs),
    /// Write <dir>/manifest.json by scanning the packed components.
    ComponentsManifest(ComponentsManifestArgs),
    /// Assemble the update-server tarball: manifest.json + files/<path>.
    Tarball(TarballArgs),
    /// Prepare an assembled drive-root for the burned image: drop default-off
    /// feature components + seed the default feature set into platforms.json.
    ImagePrep(ImagePrepArgs),
    /// (Re)write build.ninja describing the whole artifact graph.
    GenNinja,
    /// (Re)write the Makefile from loader.toml [make] (committed; triggers xtask).
    GenMakefile,
    /// (Re)write build.ninja then run ninja for the given targets (default: image).
    Build { targets: Vec<String> },
    /// Upload an already-built update tarball to a web-agency webspace.
    Upload(UploadArgs),
}

#[derive(clap::Args)]
struct UploadArgs {
    #[arg(default_value = "dist/plan-ai-update.tar.gz")]
    tarball: PathBuf,
    #[arg(long, env = "WEB_AGENCY_TOKEN")]
    token: String,
    #[arg(long, env = "WEB_AGENCY_URL")]
    url: String,
    #[arg(long, env = "WEB_AGENCY_WEBSPACE_ID")]
    webspace_id: Option<String>,
    #[arg(long, env = "WEB_AGENCY_BRANCH")]
    branch: Option<String>,
    #[arg(long, default_value = "3")]
    poll_interval: u64,
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Args)]
struct ComponentsManifestArgs {
    #[arg(default_value = "dist/components")]
    dir: PathBuf,
}

#[derive(clap::Args)]
struct ManifestArgs {
    root: PathBuf,
    #[arg(long, default_value = "0.0.0")]
    version: String,
    #[arg(long, default_value = "")]
    commit: String,
    /// Override the update URL (default: loader.toml [manifest].update_url).
    #[arg(long, default_value = "")]
    url: String,
    #[arg(long, default_value = "")]
    built_at: String,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args)]
struct ImagePrepArgs {
    root: PathBuf,
}

#[derive(clap::Args)]
struct TarballArgs {
    root: PathBuf,
    out: PathBuf,
    #[arg(long, default_value = "0.0.0")]
    version: String,
    #[arg(long, default_value = "")]
    commit: String,
    #[arg(long, default_value = "")]
    url: String,
    #[arg(long, default_value = "")]
    built_at: String,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::GenManifest(a) => gen_manifest(a),
        Cmd::ImagePrep(a) => image_prep(a),
        Cmd::ComponentsManifest(a) => components_manifest(a),
        Cmd::Tarball(a) => tarball(a),
        Cmd::GenNinja => {
            write_ninja()?;
            eprintln!("==> wrote build.ninja");
            Ok(())
        }
        Cmd::GenMakefile => gen_makefile(),
        Cmd::Build { targets } => build(targets),
        Cmd::Upload(a) => upload(a),
    }
}

// ─────────────────────────── loader.toml + repo root ───────────────────────────

/// The PROJECT root: PLANAI_REPO_ROOT (exported by the Makefile before ninja) else cwd.
fn repo_root() -> PathBuf {
    std::env::var_os("PLANAI_REPO_ROOT").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn load_cfg() -> Result<(Config, PathBuf)> {
    let root = repo_root();
    let path = root.join("loader.toml");
    let txt = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg = loader_engine::parse(&txt).with_context(|| format!("parsing {}", path.display()))?;
    Ok((cfg, root))
}

/// The loader scripts dir the emitted graph's `@loader/` tokens resolve to. Defaults
/// to the vendored submodule path; overridable for the in-tree byte-parity check.
fn loader_dir() -> String {
    std::env::var("PLANAI_LOADER_DIR").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "third_party/loader/scripts".into())
}

/// A top-level string array from usb.lock (e.g. `.targets`); None if absent.
fn lock_array(root: &Path, key: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("usb.lock")).ok()?).ok()?;
    v.get(key)?.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

fn resolve_targets(cfg: &Config, root: &Path) -> Result<Vec<String>> {
    let env_val = cfg.targets.select_env.as_ref().and_then(|k| std::env::var(k).ok());
    let lock = cfg.targets.select_lock.as_ref().and_then(|k| lock_array(root, k));
    cfg.resolve_targets(env_val.as_deref(), lock.as_deref()).map_err(|e| anyhow::anyhow!(e))
}

/// Build the manifest classification table from loader.toml's `[manifest.classify]`
/// + `[manifest.feature_classify]` + `[targets].known`.
fn classify_table(cfg: &Config) -> ClassifyTable {
    ClassifyTable {
        exact: cfg.manifest.classify.exact.clone(),
        group_targets: cfg.targets.known.clone(),
        substring: cfg.manifest.classify.substring.clone(),
        tools_targets: cfg.manifest.classify.tools.clone(),
        // Derived from each [[component]].feature (declared at the build step), merged
        // with any explicit [manifest.feature_classify] overrides — no basename guessing.
        feature: cfg.feature_classify_table(),
    }
}

fn default_features(cfg: &Config) -> Vec<String> {
    cfg.features.iter().filter(|f| f.default).map(|f| f.name.clone()).collect()
}

/// The update URL: an explicit --url override, else loader.toml [manifest].update_url.
fn effective_url(cfg: &Config, arg: &str) -> String {
    if arg.is_empty() {
        cfg.manifest.update_url.clone()
    } else {
        arg.to_string()
    }
}

// ─────────────────────────── gen-ninja / build ───────────────────────────

fn write_ninja() -> Result<()> {
    let (cfg, root) = load_cfg()?;
    let targets = resolve_targets(&cfg, &root)?;
    let ollama = cfg.flavour_keys_for("ollama", &targets);
    eprintln!("==> graph platforms: {} | ollama: {}", targets.join(" "), ollama.join(" "));
    let (srcs, tree) = loader_engine::fs_src_fns(root.clone());
    let f = loader_engine::SrcFns { srcs: &srcs, tree: &tree };
    let ninja = loader_engine::render(&cfg, &targets, &loader_dir(), &f);
    std::fs::write(root.join("build.ninja"), ninja).context("writing build.ninja")?;
    Ok(())
}

fn gen_makefile() -> Result<()> {
    let (cfg, root) = load_cfg()?;
    let mk = loader_engine::render_makefile(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    let out = root.join("Makefile");
    std::fs::write(&out, mk).with_context(|| format!("writing {}", out.display()))?;
    eprintln!("==> wrote {}", out.display());
    Ok(())
}

fn build(mut targets: Vec<String>) -> Result<()> {
    write_ninja()?;
    if targets.is_empty() {
        targets.push("image".into());
    }
    // Export PLANAI_REPO_ROOT so the vendored loader scripts (third_party/loader/scripts)
    // keep REPO_ROOT = the project root rather than re-rooting to the submodule via
    // $SCRIPT_DIR/.. (see scripts/lib.sh). The engine already runs from the project root.
    let mut cmd = Command::new("ninja");
    cmd.env("PLANAI_REPO_ROOT", repo_root()).arg("-f").arg("build.ninja").args(&targets);
    run(&mut cmd).context("ninja build")
}

// ─────────────────────────── components manifest ───────────────────────────

fn components_manifest(a: ComponentsManifestArgs) -> Result<()> {
    let (cfg, root) = load_cfg()?;
    let dir = &a.dir;
    if !dir.is_dir() {
        bail!("{} is not a directory — pack the components first", dir.display());
    }
    let present = |base: &str| -> bool {
        ["squashfs", "dmg", "tar.gz", "zip"].iter().any(|e| dir.join(format!("{base}.{e}")).is_file()) || dir.join(base).is_dir()
    };
    let runtimes: Vec<String> = cfg.targets.known.iter().filter(|t| present(&format!("runtime-{t}"))).cloned().collect();
    let ollama_keys: Vec<String> = cfg.flavours.get("ollama").map(|f| f.keys.clone()).unwrap_or_default();
    let llamacpp_keys: Vec<String> = cfg.flavours.get("llamacpp").map(|f| f.keys.clone()).unwrap_or_default();
    let ollama: Vec<String> = ollama_keys.into_iter().filter(|k| present(&format!("ollama-{k}"))).collect();
    let llamacpp: Vec<String> = llamacpp_keys.into_iter().filter(|k| present(&format!("llamacpp-{k}"))).collect();

    let manifest = serde_json::json!({
        "ollama_tag": ollama_tag(&root)?,
        "ow_assets": "ow-assets",
        "runtimes": runtimes,
        "ollama": ollama,
        "llamacpp": llamacpp,
        "note": "loader picks runtime-<this bundles OS> + ollama by CPU arch (rocm if /dev/kfd); mounts .squashfs (else extracts), extracts .tar.gz",
    });
    let out = dir.join("manifest.json");
    std::fs::write(&out, serde_json::to_string_pretty(&manifest)?).with_context(|| format!("writing {}", out.display()))?;
    eprintln!(
        "==> components manifest -> {} ({} runtimes, {} ollama)",
        out.display(),
        manifest["runtimes"].as_array().map(|a| a.len()).unwrap_or(0),
        manifest["ollama"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    Ok(())
}

/// The ollama release tag from usb.lock (`.ollama.version`).
fn ollama_tag(root: &Path) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("usb.lock")).context("reading usb.lock")?).context("parsing usb.lock")?;
    v.get("ollama").and_then(|o| o.get("version")).and_then(|s| s.as_str()).map(str::to_string).context("usb.lock: missing .ollama.version")
}

// ─────────────────────────── manifests + image-prep ───────────────────────────

fn image_prep(a: ImagePrepArgs) -> Result<()> {
    let (cfg, _root) = load_cfg()?;
    let root = &a.root;
    let manifest_path = root.join("update.json");
    let m = loader_manifest::Manifest::from_json(&std::fs::read_to_string(&manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?)?;
    let default_on = default_features(&cfg);
    let mut dropped = 0usize;
    let mut bytes = 0u64;
    for e in &m.files {
        let Some(f) = &e.feature else { continue };
        if default_on.iter().any(|d| d == f) || e.is_dir() {
            continue;
        }
        let p = root.join(&e.path);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
            dropped += 1;
            bytes += e.size.unwrap_or(0);
        }
        if let Some(t) = e.target.as_deref() {
            let tp = root.join(t);
            if tp.is_dir() {
                std::fs::remove_dir_all(&tp).with_context(|| format!("removing {}", tp.display()))?;
                dropped += 1;
            }
        }
    }
    let plats = root.join("platforms.json");
    if let Ok(txt) = std::fs::read_to_string(&plats) {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&txt) {
            v["features"] = serde_json::json!(default_on);
            std::fs::write(&plats, serde_json::to_string(&v)?)?;
        }
    }
    eprintln!("==> image-prep: dropped {dropped} default-off feature artifact(s) ({} MiB); features -> {default_on:?}", bytes / (1024 * 1024));
    Ok(())
}

fn gen_manifest(a: ManifestArgs) -> Result<()> {
    let (cfg, _root) = load_cfg()?;
    let table = classify_table(&cfg);
    let url = effective_url(&cfg, &a.url);
    let m = loader_manifest::generate(&a.root, &cfg.manifest.product, &a.version, &a.commit, &url, &a.built_at, &table)
        .with_context(|| format!("scanning {}", a.root.display()))?;
    let json = m.to_json_pretty();
    match a.out {
        Some(p) => std::fs::write(&p, json).with_context(|| format!("writing {}", p.display()))?,
        None => println!("{json}"),
    }
    Ok(())
}

fn tarball(a: TarballArgs) -> Result<()> {
    let (cfg, _root) = load_cfg()?;
    let table = classify_table(&cfg);
    let url = effective_url(&cfg, &a.url);
    let root = std::fs::canonicalize(&a.root).with_context(|| format!("canonicalize {}", a.root.display()))?;
    let m = loader_manifest::generate(&root, &cfg.manifest.product, &a.version, &a.commit, &url, &a.built_at, &table)
        .with_context(|| format!("scanning {}", root.display()))?;
    let stage = a.out.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    std::os::unix::fs::symlink(&root, stage.join("files"))?;
    std::fs::write(stage.join("manifest.json"), m.to_json_pretty())?;
    std::fs::write(stage.join("404.html"), landing_404(&cfg, &url))?;
    if let Some(parent) = a.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new("tar");
    cmd.arg("-C").arg(&stage);
    if which::which("pigz").is_ok() {
        cmd.args(["-I", "pigz"]);
    } else {
        cmd.arg("-z");
    }
    cmd.arg("-chf").arg(&a.out).arg("manifest.json").arg("404.html").arg("files");
    run(&mut cmd).context("creating tarball")?;
    let _ = std::fs::remove_dir_all(&stage);
    eprintln!("==> update tarball -> {} ({} files)", a.out.display(), m.files.len());
    Ok(())
}

/// The update host's 404 / landing page. Links come from loader.toml `[layout]`
/// (launcher_artifact map), so the page is product-agnostic.
fn landing_404(cfg: &Config, base: &str) -> String {
    let base = base.trim_end_matches('/');
    let product = &cfg.manifest.product;
    let mut links = String::new();
    if let Some(layout) = &cfg.layout {
        if let Some(map) = layout.get("launcher_artifact").and_then(|v| v.as_table()) {
            let mut entries: Vec<(&String, &str)> = map.iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s))).collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (target, file) in entries {
                links.push_str(&format!("      <li><a href=\"{base}/files/{file}\"><span>{product} — {target}</span><span class=\"os\">{file}</span></a></li>\n"));
            }
        }
    }
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{product} update server — 404</title>
<style>
  :root {{
    --bg:#f0ede4; --surface:#fff; --fg:#161a22; --fg-strong:#0b0f15;
    --fg-muted:#686a6e; --line:#e0dcd1; --brand:#ea580c; --brand-strong:#c2410c;
    --sans:ui-sans-serif,-apple-system,BlinkMacSystemFont,"Inter Tight",Inter,system-ui,sans-serif;
    --mono:"JetBrains Mono",ui-monospace,"SF Mono",Menlo,Consolas,monospace;
  }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         background:var(--bg); color:var(--fg); font-family:var(--sans); line-height:1.55; padding:2rem; }}
  .card {{ background:var(--surface); border:1px solid var(--line); border-radius:14px;
          max-width:34rem; width:100%; padding:2.25rem 2.5rem; }}
  .tag {{ font-family:var(--mono); font-size:.72rem; letter-spacing:.09em; text-transform:uppercase;
         color:var(--brand); margin:0 0 .6rem; }}
  h1 {{ font-size:1.9rem; margin:0; color:var(--fg-strong); letter-spacing:-.01em; }}
  p {{ color:var(--fg-muted); margin:.65rem 0 0; }}
  code {{ font-family:var(--mono); font-size:.85em; }}
  .dl {{ list-style:none; padding:0; margin:1.6rem 0 0; display:grid; gap:.55rem; }}
  .dl a {{ display:flex; justify-content:space-between; align-items:center; gap:1rem;
          text-decoration:none; color:var(--fg); font-weight:600;
          border:1px solid var(--line); border-radius:10px; padding:.8rem 1rem; }}
  .dl a:hover {{ border-color:var(--brand); color:var(--brand-strong); }}
  .dl .os {{ font-family:var(--mono); font-size:.76rem; color:var(--fg-muted); font-weight:400; }}
  footer {{ margin-top:1.6rem; font-family:var(--mono); font-size:.7rem; color:var(--fg-muted); }}
</style>
</head>
<body>
  <main class="card">
    <p class="tag">404 · not found</p>
    <h1>{product} update server</h1>
    <p>This host serves the offline update bundle: the launcher fetches
       <code>manifest.json</code> and the files under <code>/files/</code> to update
       itself in place. There is nothing to browse at this path.</p>
    <p>Download the launcher for your platform:</p>
    <ul class="dl">
{links}    </ul>
    <footer>{base}</footer>
  </main>
</body>
</html>
"##
    )
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawning {cmd:?}"))?;
    if !status.success() {
        bail!("command failed ({status}): {cmd:?}");
    }
    Ok(())
}

// ── web-agency upload (HTTP protocol adopted from mac-mgmt's web-agency-upload) ──

#[derive(serde::Deserialize)]
struct Whoami {
    #[allow(dead_code)]
    kind: String,
    webspace_id: Option<String>,
    webspace_name: Option<String>,
}
#[derive(serde::Deserialize)]
struct UploadResp {
    deployment_id: String,
    status: String,
}
#[derive(serde::Deserialize)]
struct StatusResp {
    #[allow(dead_code)]
    deployment_id: String,
    status: String,
    error_message: Option<String>,
}

fn upload(a: UploadArgs) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(3600)).build().context("failed to build HTTP client")?;
    deploy(&client, &a)
}

fn deploy(client: &reqwest::blocking::Client, a: &UploadArgs) -> Result<()> {
    let base = a.url.trim_end_matches('/');
    let who = whoami(client, base, &a.token)?;
    let webspace = match &a.webspace_id {
        Some(id) => {
            if let Some(scoped) = who.webspace_id.as_deref() {
                if scoped != id {
                    bail!(
                        "token is scoped to webspace {scoped} ({}), not {id} — unset \
                         WEB_AGENCY_WEBSPACE_ID to deploy to the token's webspace, or use a \
                         token authorised for {id}",
                        who.webspace_name.as_deref().unwrap_or("unknown")
                    );
                }
            }
            id.clone()
        }
        None => who
            .webspace_id
            .clone()
            .inspect(|id| eprintln!("using token's scoped webspace: {} ({id})", who.webspace_name.as_deref().unwrap_or("unknown")))
            .ok_or_else(|| anyhow::anyhow!("token not scoped to a webspace — pass --webspace-id"))?,
    };
    if !a.tarball.is_file() {
        bail!("{} not found — run `make update-tarball` first", a.tarball.display());
    }
    let mut url = format!("{base}/api/v1/deploy/{webspace}");
    if let Some(b) = &a.branch {
        url.push_str(&format!("?branch={}", percent(b)));
    }
    eprintln!("uploading {} to webspace {webspace}…", a.tarball.display());
    let body = reqwest::blocking::Body::from(std::fs::File::open(&a.tarball)?);
    let resp = client.post(&url).bearer_auth(&a.token).body(body).send().context("upload failed")?;
    if !resp.status().is_success() {
        let code = resp.status();
        let txt = resp.text().unwrap_or_default();
        bail!("upload failed ({code}): {txt}");
    }
    let up: UploadResp = resp.json().context("decoding upload response")?;
    eprintln!("deployment {} status: {}", up.deployment_id, up.status);
    if a.no_wait {
        return Ok(());
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(a.poll_interval));
        let st: StatusResp = client
            .get(format!("{base}/api/v1/deploy/{webspace}/{}", up.deployment_id))
            .bearer_auth(&a.token)
            .send()
            .context("polling status")?
            .json()
            .context("decoding status")?;
        eprintln!("  status: {}", st.status);
        match st.status.as_str() {
            "success" | "deployed" | "ready" => {
                eprintln!("==> deployed");
                return Ok(());
            }
            "failed" | "error" => bail!("deployment failed: {}", st.error_message.unwrap_or_default()),
            _ => {}
        }
    }
}

fn whoami(client: &reqwest::blocking::Client, base: &str, token: &str) -> Result<Whoami> {
    let resp = client.get(format!("{base}/api/v1/whoami")).bearer_auth(token).send().context("whoami request failed")?;
    if !resp.status().is_success() {
        bail!("whoami failed ({}) — bad token?", resp.status());
    }
    resp.json().context("decoding whoami")
}

fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}
