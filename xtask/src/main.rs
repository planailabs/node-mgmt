//! plan.ai USB build orchestrator. The Makefile is the user entrypoint; it shells
//! to this tool (built offline by nix: `nix run .#xtask`). xtask owns the logic
//! that's painful/unsafe in bash — update-manifest generation (shared with the
//! launcher updater via plan-ai-manifest) and the update-server tarball — and (next)
//! emits a ninja graph for the whole artifact build.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "plan.ai USB build orchestrator")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate an update manifest (JSON) for a drive-layout root directory.
    GenManifest(ManifestArgs),
    /// Write dist/components/manifest.json by scanning the packed components.
    ComponentsManifest(ComponentsManifestArgs),
    /// Assemble the update-server tarball: manifest.json + files/<path>.
    Tarball(TarballArgs),
    /// (Re)write build.ninja describing the whole artifact graph.
    GenNinja,
    /// (Re)write nix/stores.nix from the store paths recorded by store-import.sh.
    GenStores,
    /// (Re)write build.ninja then run ninja for the given targets (default: image).
    Build { targets: Vec<String> },
    /// Upload an already-built update tarball to a web-agency webspace.
    Upload(UploadArgs),
}

#[derive(clap::Args)]
struct UploadArgs {
    /// The update tarball to deploy (built by `make update-tarball`).
    #[arg(default_value = "dist/plan-ai-update.tar.gz")]
    tarball: PathBuf,
    /// Deploy token (web-agency).
    #[arg(long, env = "WEB_AGENCY_TOKEN")]
    token: String,
    /// Web-agency server URL.
    #[arg(long, env = "WEB_AGENCY_URL")]
    url: String,
    /// Webspace ID; if omitted, uses the token's scoped webspace.
    #[arg(long, env = "WEB_AGENCY_WEBSPACE_ID")]
    webspace_id: Option<String>,
    /// Cloudflare Pages branch (ignored for local static folders).
    #[arg(long, env = "WEB_AGENCY_BRANCH")]
    branch: Option<String>,
    /// Poll interval (seconds) while waiting for the deployment.
    #[arg(long, default_value = "3")]
    poll_interval: u64,
    /// Don't wait for the deployment to finish.
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Args)]
struct ComponentsManifestArgs {
    /// The packed components directory to scan (default: dist/components).
    #[arg(default_value = "dist/components")]
    dir: PathBuf,
}

#[derive(clap::Args)]
struct ManifestArgs {
    /// Root of the drive layout to scan (e.g. dist/bundle).
    root: PathBuf,
    #[arg(long, default_value = "0.0.0")]
    version: String,
    #[arg(long, default_value = "")]
    commit: String,
    #[arg(long, default_value = plan_ai_manifest::DEFAULT_UPDATE_URL)]
    url: String,
    /// RFC3339 build timestamp (the Makefile passes `date -u`); empty if omitted.
    #[arg(long, default_value = "")]
    built_at: String,
    /// Write here instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args)]
struct TarballArgs {
    /// Root of the drive layout to publish (e.g. dist/bundle, all platforms).
    root: PathBuf,
    /// Output tarball path (e.g. dist/plan-ai-update.tar.gz).
    out: PathBuf,
    #[arg(long, default_value = "0.0.0")]
    version: String,
    #[arg(long, default_value = "")]
    commit: String,
    #[arg(long, default_value = plan_ai_manifest::DEFAULT_UPDATE_URL)]
    url: String,
    #[arg(long, default_value = "")]
    built_at: String,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::GenManifest(a) => gen_manifest(a),
        Cmd::ComponentsManifest(a) => components_manifest(a),
        Cmd::Tarball(a) => tarball(a),
        Cmd::GenNinja => {
            write_ninja()?;
            eprintln!("==> wrote build.ninja");
            Ok(())
        }
        Cmd::GenStores => gen_stores(),
        Cmd::Build { targets } => build(targets),
        Cmd::Upload(a) => upload(a),
    }
}

/// Every platform the pipeline knows how to build. The active set for a given
/// build is a subset of this (see `resolve_targets`); nixos-x64 is not separate —
/// the linux-x64 bundle ships the FHS helper and runs on NixOS too.
const KNOWN_TARGETS: &[&str] = &["linux-x64", "win-x64", "mac-arm64"];
/// ollama flavours the loader may ship (one per CPU arch / GPU); a flavour whose
/// archive isn't vendored is simply omitted from the components manifest. The
/// active set is filtered to the OSes of the built platforms (see `ollama_keys_for`).
const KNOWN_OLLAMA: &[&str] = &["linux-amd64", "linux-arm64", "linux-amd64-rocm", "darwin", "windows-amd64"];

/// The OS family of a build target / ollama flavour, used to match flavours to the
/// platforms being built (a linux build packs the linux ollama flavours, etc.).
fn target_os(t: &str) -> &'static str {
    if t.starts_with("win") { "win" } else if t.starts_with("mac") { "mac" } else { "linux" }
}
fn flavour_os(k: &str) -> &'static str {
    if k.starts_with("windows") { "win" } else if k.starts_with("darwin") { "mac" } else { "linux" }
}

/// The platforms to build this run. Priority: PLANAI_PLATFORMS env (comma/space
/// separated, for one-off subset builds like `make image PLATFORMS=linux-x64`) >
/// usb.lock `.targets` > KNOWN_TARGETS. `nixos-x64` is folded into `linux-x64`
/// (same artifact); the result is deduped and validated against KNOWN_TARGETS.
fn resolve_targets() -> Result<Vec<String>> {
    let raw: Vec<String> = match std::env::var("PLANAI_PLATFORMS") {
        Ok(s) if !s.trim().is_empty() => s.split([',', ' ']).filter(|x| !x.is_empty()).map(str::to_string).collect(),
        _ => read_lock_array("targets").unwrap_or_else(|| KNOWN_TARGETS.iter().map(|s| s.to_string()).collect()),
    };
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let t = if t == "nixos-x64" { "linux-x64".to_string() } else { t };
        if !KNOWN_TARGETS.contains(&t.as_str()) {
            bail!("unknown platform `{t}` (known: {})", KNOWN_TARGETS.join(", "));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.is_empty() {
        bail!("no platforms selected (PLANAI_PLATFORMS / usb.lock .targets are empty)");
    }
    Ok(out)
}

/// The ollama flavours to pack for a set of built platforms: the KNOWN_OLLAMA
/// catalog filtered to the OS families present in `targets`.
fn ollama_keys_for(targets: &[String]) -> Vec<String> {
    let oses: std::collections::HashSet<&str> = targets.iter().map(|t| target_os(t)).collect();
    KNOWN_OLLAMA.iter().filter(|k| oses.contains(flavour_os(k))).map(|s| s.to_string()).collect()
}

/// Read a top-level string array from usb.lock (e.g. `.targets`); None if the file
/// or key is absent (lets the test render from defaults without a usb.lock in cwd).
fn read_lock_array(key: &str) -> Option<Vec<String>> {
    let lock = std::fs::read_to_string("usb.lock").ok()?;
    let v: serde_json::Value = serde_json::from_str(&lock).ok()?;
    v.get(key)?.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

/// Regenerate build.ninja, then run ninja for `targets` (default: image).
fn build(mut targets: Vec<String>) -> Result<()> {
    write_ninja()?;
    if targets.is_empty() {
        targets.push("image".into());
    }
    run(Command::new("ninja").arg("-f").arg("build.ninja").args(&targets)).context("ninja build")
}

/// Write nix/stores.nix mapping each imperatively-imported folder (recorded under
/// dist/.stores/<name> by scripts/store-import.sh) to a `builtins.storePath`. This
/// is the generated "supplementary nix file" the pure build layer (nix/builds.nix)
/// imports under --impure; regenerate it whenever an import's content — hence its
/// store path — changes, so nix rebuilds the dependents.
fn gen_stores() -> Result<()> {
    let dir = PathBuf::from("dist/.stores");
    let mut entries: Vec<(String, String)> = Vec::new();
    if dir.is_dir() {
        for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let path = std::fs::read_to_string(e.path())?.trim().to_string();
            if path.is_empty() {
                continue;
            }
            if !path.starts_with("/nix/store/") {
                bail!("dist/.stores/{name} is not a store path: {path}");
            }
            entries.push((name, path));
        }
    }
    entries.sort();
    let mut s = String::new();
    s.push_str("# Generated by `xtask gen-stores` — do not edit by hand.\n");
    s.push_str("# Maps imperatively-imported folders (scripts/store-import.sh) to store\n");
    s.push_str("# paths; consumed by nix/builds.nix under --impure.\n{\n");
    for (name, path) in &entries {
        s.push_str(&format!("  {name} = builtins.storePath \"{path}\";\n"));
    }
    s.push_str("}\n");
    std::fs::create_dir_all("nix")?;
    std::fs::write("nix/stores.nix", s).context("writing nix/stores.nix")?;
    eprintln!("==> wrote nix/stores.nix ({} imports)", entries.len());
    Ok(())
}

/// Write the rendered ninja graph to build.ninja for the resolved platform set.
fn write_ninja() -> Result<()> {
    let targets = resolve_targets()?;
    let ollama = ollama_keys_for(&targets);
    eprintln!("==> graph platforms: {} | ollama: {}", targets.join(" "), ollama.join(" "));
    std::fs::write("build.ninja", render_ninja(&targets, &ollama)).context("writing build.ninja")
}

/// Emit a ninja graph for the whole pipeline so EVERY sub-step is dependency-tracked
/// — no stale "just there" outputs feeding a later step. Leaf commands are the
/// existing scripts / nix builds; ninja owns ordering + staleness via stamp files
/// (dist/.ninja/<step>.stamp) plus declared source inputs.
///
/// Pure render (returns the file text, the only IO being the source-tree reads for
/// input lists) so `ninja_graph_is_structurally_sound` can validate the graph
/// without writing build.ninja or invoking ninja. `targets` / `ollama_keys` are the
/// resolved active set (see `resolve_targets` / `ollama_keys_for`).
fn render_ninja(targets: &[String], ollama_keys: &[String]) -> String {
    let mut n = String::new();
    n.push_str("# Generated by `xtask gen-ninja` — do not edit by hand.\n");
    n.push_str("# Make is the entrypoint; it calls `nix run .#xtask -- build <target>`.\n\n");
    // stamp = step whose real output is a dir / nix-store path; gen = real file out.
    n.push_str("rule stamp\n  command = mkdir -p dist/.ninja && $cmd && touch $out\n  description = $desc\n\n");
    n.push_str("rule gen\n  command = $cmd\n  description = $desc\n\n");

    let stamp = |s: &str| format!("dist/.ninja/{s}.stamp");
    let mut edges = String::new();
    let mut stamp_edge = |out: &str, deps: &[String], cmd: &str, desc: &str| {
        edges.push_str(&format!("build {}: stamp", stamp(out)));
        for d in deps {
            edges.push(' ');
            edges.push_str(d);
        }
        edges.push_str(&format!("\n  cmd = {cmd}\n  desc = {desc}\n\n"));
    };

    // --- leaf/source-driven steps ------------------------------------------
    stamp_edge("download", &srcs(&["usb.lock", "vendor.lock.json"]), "./scripts/fetch-vendor.sh", "download");
    stamp_edge("wheel", &[stamp("download")], "./scripts/build-openwebui.sh", "openwebui wheel");
    stamp_edge("app", &srcs(&["app/package.json", "app/package-lock.json"]), "(cd app && npm ci)", "electron deps");
    stamp_edge("spa", &src_tree("launcher/spa-src"), "./scripts/build-spa.sh", "dioxus spa");

    // runtimes per target (need the downloaded interpreter + the wheel)
    let mut runtime_stamps = Vec::new();
    for t in targets {
        let s = stamp(&format!("runtime-{t}"));
        stamp_edge(&format!("runtime-{t}"), &[stamp("download"), stamp("wheel")], &format!("./scripts/make-runtime.sh {t}"), &format!("runtime {t}"));
        runtime_stamps.push(s);
    }

    // components: ONE pack edge per component, so ninja runs the independent
    // squashfs/dmg/dir packs in parallel and tracks each on its own (replacing
    // build-components.sh + its hand-rolled job pool). Then a manifest edge scans
    // what actually got packed. `components` stays the manifest's stamp name so the
    // bundle deps below are unchanged, and `make components` still builds the lot.
    let mut comp_stamps = Vec::new();
    // the packer + its shared primitives are inputs too, so editing them re-packs.
    let pack_srcs = srcs(&["scripts/pack-component.sh", "scripts/lib.sh"]);
    // <component-name> -> the stamp it depends on (its built source). ow-assets (the
    // sentence-transformers + nltk offline assets) is produced by the WHEEL step
    // (build-openwebui.sh), not download — depend on wheel so the pack waits for it.
    let mut comp_jobs: Vec<(String, String)> = vec![("ow-assets".into(), stamp("wheel"))];
    for t in targets {
        comp_jobs.push((format!("runtime-{t}"), stamp(&format!("runtime-{t}"))));
    }
    for k in ollama_keys {
        comp_jobs.push((format!("ollama-{k}"), stamp("download")));
    }
    for (name, src_stamp) in &comp_jobs {
        let mut deps = vec![src_stamp.clone()];
        deps.extend(pack_srcs.iter().cloned());
        stamp_edge(&format!("comp-{name}"), &deps, &format!("./scripts/pack-component.sh {name}"), &format!("pack {name}"));
        comp_stamps.push(stamp(&format!("comp-{name}")));
    }
    // manifest: scans dist/components after every pack (xtask, not bash/jq).
    stamp_edge("components", &comp_stamps, "nix run .#xtask -- components-manifest", "components manifest");

    // bundle per target: depends on components + spa + app + launcher/spinner/crate sources
    let mut bundle_src = src_tree("launcher/src");
    bundle_src.extend(src_tree("spinner/src"));
    bundle_src.extend(src_tree("crates"));
    bundle_src.extend(srcs(&["launcher/Cargo.toml", "launcher/Cargo.lock", "spinner/Cargo.toml", "spinner/Cargo.lock", "scripts/bundle.sh", "scripts/lib.sh", "flake.nix"]));
    let mut bundle_stamps = Vec::new();
    for t in targets {
        let mut deps = vec![stamp("components"), stamp("spa"), stamp("app")];
        deps.extend(bundle_src.iter().cloned());
        stamp_edge(&format!("bundle-{t}"), &deps, &format!("./scripts/bundle.sh {t}"), &format!("bundle {t}"));
        bundle_stamps.push(stamp(&format!("bundle-{t}")));
    }

    // models: pre-pull the usb.lock models into ./models (a persistent cache like
    // vendor/ — survives `make clean`, idempotent re-pulls) so the image ships them
    // for offline use. Needs the vendored ollama binary (download); re-seeds when the
    // model list in usb.lock changes. The image depends on it, so `make image` always
    // bakes the models in (no more manual `make seed` step).
    let mut models_deps = vec![stamp("download")];
    models_deps.extend(srcs(&["usb.lock", "scripts/seed-models.sh", "scripts/lib.sh"]));
    stamp_edge("models", &models_deps, "./scripts/seed-models.sh", "seed models");

    // image + tarball: real-file outputs, depend on every bundle + their scripts.
    // The image also depends on models/ (baked into the drive); the update tarball
    // does NOT — models are excluded from the update manifest.
    let mut img_deps = bundle_stamps.clone();
    img_deps.push(stamp("models"));
    img_deps.extend(srcs(&["scripts/make-usb-image.sh"]));
    img_deps.extend(src_tree("xtask/src"));
    img_deps.extend(src_tree("crates"));
    edges.push_str(&format!("build dist/plan-ai-usb.img: gen {}\n  cmd = ./scripts/make-usb-image.sh\n  desc = usb image\n\n", img_deps.join(" ")));
    let mut tar_deps = bundle_stamps.clone();
    tar_deps.extend(srcs(&["scripts/make-update-tarball.sh"]));
    tar_deps.extend(src_tree("xtask/src"));
    tar_deps.extend(src_tree("crates"));
    edges.push_str(&format!("build dist/plan-ai-update.tar.gz: gen {}\n  cmd = ./scripts/make-update-tarball.sh\n  desc = update tarball\n\n", tar_deps.join(" ")));

    // phony aliases so `ninja <name>` (and the Makefile) read naturally
    for s in ["download", "wheel", "app", "spa", "components", "models"] {
        edges.push_str(&format!("build {s}: phony {}\n", stamp(s)));
    }
    // per-component packs (handy for `ninja comp-ollama-darwin` while iterating)
    edges.push_str(&format!("build comp-ow-assets: phony {}\n", stamp("comp-ow-assets")));
    for t in targets {
        edges.push_str(&format!("build comp-runtime-{t}: phony {}\n", stamp(&format!("comp-runtime-{t}"))));
    }
    for k in ollama_keys {
        edges.push_str(&format!("build comp-ollama-{k}: phony {}\n", stamp(&format!("comp-ollama-{k}"))));
    }
    for t in targets {
        edges.push_str(&format!("build runtime-{t}: phony {}\n", stamp(&format!("runtime-{t}"))));
        edges.push_str(&format!("build bundle-{t}: phony {}\n", stamp(&format!("bundle-{t}"))));
    }
    edges.push_str(&format!("build runtimes: phony {}\n", runtime_stamps.join(" ")));
    edges.push_str(&format!("build bundles: phony {}\n", bundle_stamps.join(" ")));
    edges.push_str("build image: phony dist/plan-ai-usb.img\n");
    edges.push_str("build update-tarball: phony dist/plan-ai-update.tar.gz\n");
    edges.push_str("build all: phony dist/plan-ai-usb.img\n");
    edges.push_str("default image\n");

    n.push_str(&edges);
    n
}

/// Existing files from the list (skip missing — e.g. an optional lock).
fn srcs(paths: &[&str]) -> Vec<String> {
    paths.iter().filter(|p| PathBuf::from(p).exists()).map(|p| p.to_string()).collect()
}

/// All source files under `root` (recursively), skipping build dirs. Used as ninja
/// inputs so editing code re-triggers the dependent steps.
fn src_tree(root: &str) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name == ".git" || name == "node_modules" {
                continue;
            }
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(&PathBuf::from(root), &mut out);
    out.sort();
    out
}

/// Write `<dir>/manifest.json` describing which components actually got packed.
/// Replaces the old jq tail of build-components.sh: scanning the directory keeps
/// this honest (it reports only what's really on disk) and the JSON shaping is
/// cleaner here than in bash. The loader reads this to pick its runtime + ollama
/// flavour at first launch.
fn components_manifest(a: ComponentsManifestArgs) -> Result<()> {
    let dir = &a.dir;
    if !dir.is_dir() {
        bail!("{} is not a directory — pack the components first", dir.display());
    }
    // A component is "present" in this OS-neutral pool as a file (base.squashfs /
    // base.dmg / base.tar.gz) OR a pre-extracted dir (base/).
    let present = |base: &str| -> bool {
        ["squashfs", "dmg", "tar.gz"].iter().any(|e| dir.join(format!("{base}.{e}")).is_file())
            || dir.join(base).is_dir()
    };
    // scan the full catalogs and report whatever's actually on disk, so the manifest
    // is honest regardless of which platform subset this build targeted.
    let runtimes: Vec<&str> = KNOWN_TARGETS.iter().copied().filter(|t| present(&format!("runtime-{t}"))).collect();
    let ollama: Vec<&str> = KNOWN_OLLAMA.iter().copied().filter(|k| present(&format!("ollama-{k}"))).collect();

    let manifest = serde_json::json!({
        "ollama_tag": ollama_tag()?,
        "ow_assets": "ow-assets",
        "runtimes": runtimes,
        "ollama": ollama,
        "note": "loader picks runtime-<this bundles OS> + ollama by CPU arch (rocm if /dev/kfd); mounts .squashfs (else extracts), extracts .tar.gz",
    });
    let out = dir.join("manifest.json");
    std::fs::write(&out, serde_json::to_string_pretty(&manifest)?).with_context(|| format!("writing {}", out.display()))?;
    eprintln!("==> components manifest -> {} ({} runtimes, {} ollama)", out.display(), runtimes.len(), ollama.len());
    Ok(())
}

/// The ollama release tag from usb.lock (`.ollama.version`) — the single source
/// of truth lib.sh's `ollama_version` also reads.
fn ollama_tag() -> Result<String> {
    let lock = std::fs::read_to_string("usb.lock").context("reading usb.lock")?;
    let v: serde_json::Value = serde_json::from_str(&lock).context("parsing usb.lock")?;
    v.get("ollama").and_then(|o| o.get("version")).and_then(|s| s.as_str()).map(str::to_string)
        .context("usb.lock: missing .ollama.version")
}

fn gen_manifest(a: ManifestArgs) -> Result<()> {
    let m = plan_ai_manifest::generate(&a.root, &a.version, &a.commit, &a.url, &a.built_at)
        .with_context(|| format!("scanning {}", a.root.display()))?;
    let json = m.to_json_pretty();
    match a.out {
        Some(p) => std::fs::write(&p, json).with_context(|| format!("writing {}", p.display()))?,
        None => println!("{json}"),
    }
    Ok(())
}

fn tarball(a: TarballArgs) -> Result<()> {
    // Resolve to an absolute path so the staged `files` symlink is valid wherever
    // tar runs. The root is a curated symlink mirror of the drive (the scripts
    // build it, excluding electron-builder's *-unpacked dirs).
    let root = std::fs::canonicalize(&a.root).with_context(|| format!("canonicalize {}", a.root.display()))?;
    let m = plan_ai_manifest::generate(&root, &a.version, &a.commit, &a.url, &a.built_at)
        .with_context(|| format!("scanning {}", root.display()))?;
    let stage = a.out.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    // files/ -> the mirror; `tar -h` dereferences symlinks so real content lands
    // under files/<path> (no GB copy). manifest.json sits beside it at the root.
    std::os::unix::fs::symlink(&root, stage.join("files"))?;
    std::fs::write(stage.join("manifest.json"), m.to_json_pretty())?;
    if let Some(parent) = a.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    run(Command::new("tar").arg("-C").arg(&stage).arg("-czhf").arg(&a.out).arg("manifest.json").arg("files"))
        .context("creating tarball")?;
    let _ = std::fs::remove_dir_all(&stage);
    eprintln!("==> update tarball -> {} ({} files)", a.out.display(), m.files.len());
    Ok(())
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
    deployment_id: String,
    status: String,
    error_message: Option<String>,
}

fn upload(a: UploadArgs) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::new();
    deploy(&client, &a)
}

/// whoami (if no webspace) → POST the tarball → poll status. Split out so tests can
/// drive it against a local server.
fn deploy(client: &reqwest::blocking::Client, a: &UploadArgs) -> Result<()> {
    let base = a.url.trim_end_matches('/');
    let webspace = match &a.webspace_id {
        Some(id) => id.clone(),
        None => resolve_webspace(client, base, &a.token)?,
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
        let s = resp.status();
        bail!("upload failed ({s}): {}", resp.text().unwrap_or_default());
    }
    let up: UploadResp = resp.json().context("invalid upload response")?;
    eprintln!("deployment {} started ({})", up.deployment_id, up.status);
    if a.no_wait {
        println!("{}", serde_json::json!({ "deployment_id": up.deployment_id, "status": up.status }));
        return Ok(());
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(a.poll_interval));
        let resp = client.get(format!("{base}/api/v1/deploy/{webspace}/status")).bearer_auth(&a.token).send().context("status check failed")?;
        if !resp.status().is_success() {
            eprintln!("warning: status check failed: {}", resp.text().unwrap_or_default());
            continue;
        }
        let st: StatusResp = resp.json().context("invalid status response")?;
        match st.status.as_str() {
            "success" => {
                eprintln!("deployment {} successful", st.deployment_id);
                return Ok(());
            }
            "failed" => bail!("deployment failed: {}", st.error_message.unwrap_or_else(|| "unknown error".into())),
            other => eprintln!("status: {other}"),
        }
    }
}

fn resolve_webspace(client: &reqwest::blocking::Client, base: &str, token: &str) -> Result<String> {
    let resp = client.get(format!("{base}/api/v1/deploy/whoami")).bearer_auth(token).send().context("failed to reach server")?;
    if !resp.status().is_success() {
        let s = resp.status();
        bail!("whoami failed ({s}): {}", resp.text().unwrap_or_default());
    }
    let w: Whoami = resp.json().context("invalid whoami response")?;
    if w.kind != "deploy" {
        bail!("token kind is '{}', expected 'deploy'", w.kind);
    }
    let name = w.webspace_name.as_deref().unwrap_or("unknown");
    w.webspace_id
        .inspect(|id| eprintln!("using token's scoped webspace: {name} ({id})"))
        .ok_or_else(|| anyhow::anyhow!("token not scoped to a webspace — pass --webspace-id"))
}

/// Percent-encode a path/query segment (branch names).
fn percent(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A one-shot HTTP/1.1 server that replies to each request with a canned body,
    /// consuming any Content-Length body first so the client doesn't block.
    fn serve(responses: Vec<(&'static str, &'static str)>) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resp: Vec<(String, String)> = responses.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        let h = std::thread::spawn(move || {
            for (code, body) in resp {
                let (mut sock, _) = listener.accept().unwrap();
                // Read headers, then drain a Content-Length body if present.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                let mut content_len = 0usize;
                loop {
                    let n = sock.read(&mut tmp).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        content_len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        let have = buf.len() - (p + 4);
                        let mut remaining = content_len.saturating_sub(have);
                        while remaining > 0 {
                            let n = sock.read(&mut tmp).unwrap();
                            if n == 0 {
                                break;
                            }
                            remaining = remaining.saturating_sub(n);
                        }
                        break;
                    }
                }
                let out = format!(
                    "HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(out.as_bytes()).unwrap();
            }
        });
        (format!("http://{addr}"), h)
    }

    fn test_client() -> reqwest::blocking::Client {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::blocking::Client::new()
    }

    fn args(url: String, tarball: PathBuf) -> UploadArgs {
        UploadArgs {
            tarball,
            token: "t".into(),
            url,
            webspace_id: Some("ws1".into()),
            branch: None,
            poll_interval: 0,
            no_wait: false,
        }
    }

    #[test]
    fn deploy_uploads_then_polls_to_success() {
        let tb = std::env::temp_dir().join(format!("xtask-upl-{}.tar.gz", std::process::id()));
        std::fs::write(&tb, b"fake tarball").unwrap();
        let (url, h) = serve(vec![
            ("200 OK", r#"{"deployment_id":"d1","status":"queued"}"#), // POST upload
            ("200 OK", r#"{"deployment_id":"d1","status":"success"}"#), // GET status
        ]);
        let client = test_client();
        let r = deploy(&client, &args(url, tb.clone()));
        h.join().unwrap();
        std::fs::remove_file(&tb).ok();
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn deploy_reports_failed_deployment() {
        let tb = std::env::temp_dir().join(format!("xtask-upl2-{}.tar.gz", std::process::id()));
        std::fs::write(&tb, b"fake").unwrap();
        let (url, h) = serve(vec![
            ("200 OK", r#"{"deployment_id":"d2","status":"queued"}"#),
            ("200 OK", r#"{"deployment_id":"d2","status":"failed","error_message":"boom"}"#),
        ]);
        let client = test_client();
        let r = deploy(&client, &args(url, tb.clone()));
        h.join().unwrap();
        std::fs::remove_file(&tb).ok();
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("boom"));
    }

    // ── ninja graph structure ────────────────────────────────────────────────

    struct Edge {
        outputs: Vec<String>,
        inputs: Vec<String>,
    }

    /// Parse the `build <outputs>: <rule> <inputs...>` lines from a ninja file.
    /// The generator emits exactly one output per edge and no order-only (`|`/`||`)
    /// deps, so a line split is enough — we don't need a full ninja parser.
    fn parse_edges(ninja: &str) -> Vec<Edge> {
        ninja
            .lines()
            .filter_map(|l| {
                let l = l.strip_prefix("build ")?;
                let (outs, rest) = l.split_once(": ")?;
                let mut toks = rest.split_whitespace();
                let _rule = toks.next()?; // rule name (stamp|gen|phony)
                Some(Edge {
                    outputs: outs.split_whitespace().map(str::to_string).collect(),
                    inputs: toks.map(str::to_string).collect(),
                })
            })
            .collect()
    }

    /// Validate the generated graph WITHOUT running ninja: no duplicate output
    /// edges, no dangling build-artifact dependency (every stamp / dist artifact
    /// used as an input is produced by some edge), the expected targets all exist,
    /// and `default` points at a real output. Catches the structural-typo class
    /// (renamed/forgotten edge, alias to nothing) at `cargo test` time — long
    /// before a build would surface it as a cryptic ninja error.
    #[test]
    fn ninja_graph_is_structurally_sound() {
        // validate the full graph (every known platform + ollama flavour)
        let targets: Vec<String> = KNOWN_TARGETS.iter().map(|s| s.to_string()).collect();
        let ollama_keys: Vec<String> = KNOWN_OLLAMA.iter().map(|s| s.to_string()).collect();
        let ninja = render_ninja(&targets, &ollama_keys);
        let edges = parse_edges(&ninja);
        assert!(!edges.is_empty(), "no build edges parsed");

        // collect every declared output; flag duplicates (two edges making the same)
        let mut outputs = std::collections::HashSet::new();
        for e in &edges {
            for o in &e.outputs {
                assert!(outputs.insert(o.as_str()), "duplicate output edge for `{o}`");
            }
        }

        // a build artifact = a stamp under dist/.ninja or a real dist/ output. Those
        // MUST be produced by some edge when used as an input; plain source files
        // (usb.lock, scripts/*.sh, launcher/src/*) are leaves and are skipped.
        let is_artifact = |p: &str| {
            (p.starts_with("dist/.ninja/") && p.ends_with(".stamp"))
                || p == "dist/plan-ai-usb.img"
                || p == "dist/plan-ai-update.tar.gz"
        };
        for e in &edges {
            for inp in &e.inputs {
                if is_artifact(inp) {
                    assert!(
                        outputs.contains(inp.as_str()),
                        "dangling dependency: `{inp}` (input of {:?}) is produced by no edge",
                        e.outputs
                    );
                }
            }
        }

        // the headline targets + every per-target/ollama edge must resolve
        let mut expected: Vec<String> = ["image", "update-tarball", "components", "models",
            "runtimes", "bundles", "download", "wheel", "app", "spa", "comp-ow-assets"]
            .iter().map(|s| s.to_string()).collect();
        for t in &targets {
            expected.push(format!("comp-runtime-{t}"));
            expected.push(format!("bundle-{t}"));
            expected.push(format!("runtime-{t}"));
        }
        for k in &ollama_keys {
            expected.push(format!("comp-ollama-{k}"));
        }
        for t in &expected {
            assert!(outputs.contains(t.as_str()), "expected target `{t}` missing from graph");
        }

        // `default <target>` must name a real output
        let default = ninja.lines().find_map(|l| l.strip_prefix("default ")).expect("no default line");
        assert!(
            outputs.contains(default.trim()),
            "default target `{default}` is produced by no edge"
        );
    }
}
