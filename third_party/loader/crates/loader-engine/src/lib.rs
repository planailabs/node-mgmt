//! The loader build engine: parse a project's `loader.toml` and render the ninja
//! build graph for a selected set of targets. The engine is project-agnostic — every
//! plan.ai-ism (targets, flavour catalogs, features, components, FHS package set,
//! classification, drive layout) is *data* read from `loader.toml`; this crate is the
//! generic emission logic the historical hand-written `render_ninja` is replaced by.
//!
//! `@loader/<script>` tokens in cmds/srcs are rewritten to the loader scripts dir
//! (the submodule's `scripts/`) at render time, so the same manifest works whether the
//! scripts live in-tree (`scripts/`, for the byte-parity test) or vendored
//! (`third_party/loader/scripts/`, for the real build).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ─────────────────────────── loader.toml schema ───────────────────────────

#[derive(Debug, Deserialize)]
pub struct Config {
    #[allow(dead_code)]
    pub schema: u32,
    pub manifest: ManifestId,
    #[serde(default)]
    pub layout: Option<toml::Value>,
    pub targets: Targets,
    #[serde(default, rename = "feature")]
    pub features: Vec<Feature>,
    #[serde(default)]
    pub fhs: Option<toml::Value>,
    #[serde(default)]
    pub flavours: BTreeMap<String, Flavour>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub srcgroups: BTreeMap<String, Vec<String>>,
    #[serde(rename = "component")]
    pub components: Vec<Component>,
    pub bundle: Bundle,
    #[serde(default, rename = "artifact")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, rename = "launch")]
    pub launch: Vec<toml::Value>,
    #[serde(default)]
    pub make: Option<Make>,
}

#[derive(Debug, Deserialize)]
pub struct Make {
    #[serde(default = "default_xtask_cmd")]
    pub xtask_cmd: String,
    #[serde(default = "default_devshell_var")]
    pub devshell_var: String,
    #[serde(default)]
    pub guard_exclude: Vec<String>,
    #[serde(default = "default_goal")]
    pub default_goal: String,
    #[serde(default, rename = "target")]
    pub targets: Vec<MakeTarget>,
}

fn default_xtask_cmd() -> String {
    "nix run .#xtask --".into()
}
fn default_devshell_var() -> String {
    "PLANAI_DEVSHELL".into()
}
fn default_goal() -> String {
    "all".into()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct MakeTarget {
    pub name: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub recipe: Vec<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default = "default_true")]
    pub phony: bool,
}

#[derive(Debug, Deserialize)]
pub struct ManifestId {
    pub product: String,
    pub update_url: String,
    #[serde(default)]
    pub classify: ClassifySpec,
    #[serde(default)]
    pub feature_classify: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub image_prep: Option<toml::Value>,
    #[serde(default)]
    pub upload: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ClassifySpec {
    #[serde(default)]
    pub exact: Vec<(String, String)>,
    #[serde(default)]
    pub substring: Vec<(String, Vec<String>)>,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Targets {
    pub known: Vec<String>,
    #[serde(default)]
    pub select_env: Option<String>,
    #[serde(default)]
    pub select_lock: Option<String>,
    #[serde(default)]
    pub alias: BTreeMap<String, String>,
    #[serde(default)]
    pub os_match: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Feature {
    pub name: String,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Deserialize)]
pub struct Flavour {
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Step {
    pub name: String,
    #[serde(default)]
    pub per: Option<String>,
    pub cmd: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub src: Vec<String>,
    #[serde(default)]
    pub src_tree: Vec<String>,
    pub desc: String,
}

#[derive(Debug, Deserialize)]
pub struct Component {
    pub name: String,
    /// The optional feature this component's outputs belong to. Empty or "core" =
    /// no feature (always wanted). Drives manifest feature-tagging authoritatively —
    /// no basename guessing. See `Config::feature_classify_table`.
    #[serde(default)]
    pub feature: String,
    #[serde(default)]
    pub per: String,
    #[serde(default)]
    pub flavour: Option<String>,
    pub group: u8,
    pub builder: String,
    #[serde(default)]
    pub src_stamp: Option<String>,
    #[serde(default)]
    pub store_name: Option<String>,
    #[serde(default)]
    pub src_dir: Option<String>,
    #[serde(default)]
    pub srcgroup: Option<String>,
    #[serde(default)]
    pub stamp: Option<String>,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub attr: Option<String>,
    #[serde(default)]
    pub out: Option<String>,
    #[serde(default)]
    pub shared_os: Option<String>,
    #[serde(default)]
    pub src_extra: Vec<String>,
    #[serde(default)]
    pub src_tree: Vec<String>,
    #[serde(default)]
    pub format: BTreeMap<String, Format>,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    pub ext: String,
    #[serde(default)]
    pub attr: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub out: Option<String>,
    #[serde(default)]
    pub stamp: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Bundle {
    pub cmd: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub src: Vec<String>,
    #[serde(default)]
    pub src_tree: Vec<String>,
    pub desc: String,
}

#[derive(Debug, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub out: String,
    pub cmd: String,
    #[serde(default)]
    pub deps_bundles: bool,
    #[serde(default)]
    pub deps_steps: Vec<String>,
    #[serde(default)]
    pub src: Vec<String>,
    #[serde(default)]
    pub src_tree: Vec<String>,
    pub desc: String,
    #[serde(default)]
    pub default: bool,
}

// ─────────────────────────── parse + resolve ───────────────────────────

pub fn parse(toml_str: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(toml_str)
}

impl Config {
    /// OS family of a target/flavour key, per `[targets.os_match]` (default linux).
    pub fn os_of(&self, key: &str) -> &'static str {
        for (os, needles) in &self.os_match_pairs() {
            if needles.iter().any(|n| key.starts_with(n.as_str())) {
                return match os.as_str() {
                    "win" => "win",
                    "mac" => "mac",
                    _ => "linux",
                };
            }
        }
        "linux"
    }
    fn os_match_pairs(&self) -> Vec<(String, Vec<String>)> {
        // deterministic order: win then mac (linux is the default fallthrough)
        let mut v = Vec::new();
        for os in ["win", "mac"] {
            if let Some(n) = self.targets.os_match.get(os) {
                v.push((os.to_string(), n.clone()));
            }
        }
        v
    }

    /// The active target set: env (comma/space list) > lock array > known. Aliases
    /// folded; validated against `known`; deduped, order-preserving.
    pub fn resolve_targets(&self, env_val: Option<&str>, lock_targets: Option<&[String]>) -> Result<Vec<String>, String> {
        let raw: Vec<String> = match env_val {
            Some(s) if !s.trim().is_empty() => s.split([',', ' ']).filter(|x| !x.is_empty()).map(str::to_string).collect(),
            _ => match lock_targets {
                Some(l) if !l.is_empty() => l.to_vec(),
                _ => self.targets.known.clone(),
            },
        };
        let mut out: Vec<String> = Vec::new();
        for t in raw {
            let t = self.targets.alias.get(&t).cloned().unwrap_or(t);
            if !self.targets.known.contains(&t) {
                return Err(format!("unknown platform `{t}` (known: {})", self.targets.known.join(", ")));
            }
            if !out.contains(&t) {
                out.push(t);
            }
        }
        if out.is_empty() {
            return Err("no platforms selected".into());
        }
        Ok(out)
    }

    /// A flavour catalog filtered to the OS families present in `targets`.
    pub fn flavour_keys_for(&self, flavour: &str, targets: &[String]) -> Vec<String> {
        let oses: Vec<&'static str> = targets.iter().map(|t| self.os_of(t)).collect();
        match self.flavours.get(flavour) {
            Some(f) => f.keys.iter().filter(|k| oses.contains(&self.os_of(k))).cloned().collect(),
            None => Vec::new(),
        }
    }

    /// Feature → output-basename-prefix(es), derived from the `[[component]].feature`
    /// declared at the build step (authoritative — no basename guessing). A component
    /// whose feature is empty or "core" contributes nothing (its files are core/None).
    /// Any explicit `[manifest.feature_classify]` entries are merged on top, so a
    /// project keeps an escape hatch for files no component produces (or to split a
    /// component's outputs across features by overriding specific prefixes).
    pub fn feature_classify_table(&self) -> Vec<(String, Vec<String>)> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for c in &self.components {
            if c.feature.is_empty() || c.feature == "core" {
                continue;
            }
            for p in self.component_tag_prefixes(c) {
                map.entry(c.feature.clone()).or_default().push(p);
            }
        }
        for (f, prefixes) in &self.manifest.feature_classify {
            map.entry(f.clone()).or_default().extend(prefixes.iter().cloned());
        }
        for v in map.values_mut() {
            v.sort();
            v.dedup();
        }
        map.into_iter().collect()
    }

    /// The output-basename prefix(es) a component's files match (for feature tagging).
    /// Explicit `out` (the shared sqfs/dmg) → its basename stem; per shared-os → each
    /// format's out stem; per target/flavour → `<name>-`; else (a shared pack) → `<name>`.
    fn component_tag_prefixes(&self, c: &Component) -> Vec<String> {
        if let Some(out) = &c.out {
            return vec![basename_stem(out)];
        }
        match c.per.as_str() {
            "shared-os" => c.format.values().filter_map(|f| f.out.as_deref()).map(basename_stem).collect(),
            "target" | "flavour" => vec![format!("{}-", c.name)],
            _ => vec![c.name.clone()],
        }
    }

    fn active_oses(&self, targets: &[String]) -> Vec<&'static str> {
        let mut v = Vec::new();
        for t in targets {
            let os = self.os_of(t);
            if !v.contains(&os) {
                v.push(os);
            }
        }
        v
    }
}

// ─────────────────────────── token resolution ───────────────────────────

/// Resolve a *command* token: `@loader/x` → `./<loader_dir>/x` (executable form).
fn cmd_resolve(s: &str, loader_dir: &str) -> String {
    s.replace("@loader/", &format!("./{loader_dir}/"))
}

/// Resolve a *dependency/source* token: `@loader/x` → `<loader_dir>/x`, strip a
/// leading `./` (ninja deps are written without it).
fn dep_resolve(s: &str, loader_dir: &str) -> String {
    let s = s.replace("@loader/", &format!("{loader_dir}/"));
    s.strip_prefix("./").map(str::to_string).unwrap_or(s)
}

// ─────────────────────────── render ───────────────────────────

/// The source-listing functions the render depends on, injected so the pure logic
/// can be tested and so the real xtask can read the live working tree.
pub struct SrcFns<'a> {
    /// Existing files from a list (skip missing); paths already dep-resolved.
    pub srcs: &'a dyn Fn(&[String]) -> Vec<String>,
    /// All source files under a tree root (recursive, sorted, skipping build dirs).
    pub tree: &'a dyn Fn(&str) -> Vec<String>,
}

/// Filesystem-backed `SrcFns` rooted at `repo_root` (what the real build uses).
pub fn fs_src_fns(repo_root: PathBuf) -> (impl Fn(&[String]) -> Vec<String>, impl Fn(&str) -> Vec<String>) {
    let r1 = repo_root.clone();
    let srcs = move |paths: &[String]| -> Vec<String> {
        paths.iter().filter(|p| r1.join(p).exists()).map(|p| p.to_string()).collect()
    };
    let r2 = repo_root;
    let tree = move |root: &str| -> Vec<String> {
        let mut out = Vec::new();
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name == "target" || name == ".git" || name == "node_modules" {
                    continue;
                }
                if p.is_dir() {
                    walk(base, &p, out);
                } else if let Ok(rel) = p.strip_prefix(base) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        walk(&r2, &r2.join(root), &mut out);
        out.sort();
        out
    };
    (srcs, tree)
}

fn stamp(s: &str) -> String {
    format!("dist/.ninja/{s}.stamp")
}

/// The basename of a path with its final extension stripped
/// (`dist/components/ow-assets.squashfs` → `ow-assets`).
fn basename_stem(out: &str) -> String {
    let base = out.rsplit('/').next().unwrap_or(out);
    match base.rsplit_once('.') {
        Some((stem, _)) => stem.to_string(),
        None => base.to_string(),
    }
}

/// Emit one `stamp`-rule edge.
fn se(edges: &mut String, out: &str, deps: &[String], cmd: &str, desc: &str) {
    edges.push_str(&format!("build {}: stamp", stamp(out)));
    for d in deps {
        edges.push(' ');
        edges.push_str(d);
    }
    edges.push_str(&format!("\n  cmd = {cmd}\n  desc = {desc}\n\n"));
}

/// Render the full ninja graph for `targets`. `loader_dir` is the scripts dir the
/// `@loader/` tokens resolve to (`scripts` in-tree for the parity test;
/// `third_party/loader/scripts` for the vendored build).
pub fn render(cfg: &Config, targets: &[String], loader_dir: &str, f: &SrcFns) -> String {
    let ollama_keys = cfg.flavour_keys_for("ollama", targets);
    let active_oses = cfg.active_oses(targets);

    let mut n = String::new();
    n.push_str("# Generated by `xtask gen-ninja` — do not edit by hand.\n");
    n.push_str("# Make is the entrypoint; it calls `nix run .#xtask -- build <target>`.\n\n");
    n.push_str("rule stamp\n  command = mkdir -p dist/.ninja && $cmd && touch $out\n  description = $desc\n\n");
    n.push_str("rule gen\n  command = $cmd\n  description = $desc\n\n");

    let mut edges = String::new();

    // deps from a list, dep-resolved + existence-filtered.
    let resolve_srcs = |list: &[String]| -> Vec<String> {
        let resolved: Vec<String> = list.iter().map(|s| dep_resolve(s, loader_dir)).collect();
        (f.srcs)(&resolved)
    };
    let tree_all = |roots: &[String]| -> Vec<String> {
        let mut out = Vec::new();
        for r in roots {
            out.extend((f.tree)(r));
        }
        out
    };
    let step = |name: &str| -> &Step { cfg.steps.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("loader.toml missing [[step]] {name}")) };
    // a leaf step (download/wheel/app/spa/models): deps = dep-stamps + srcs + tree
    let emit_step = |edges: &mut String, s: &Step, target: Option<&str>| {
        let name = match target {
            Some(t) => format!("{}-{t}", s.name),
            None => s.name.clone(),
        };
        let mut deps: Vec<String> = s.deps.iter().map(|d| stamp(d)).collect();
        deps.extend(resolve_srcs(&s.src));
        deps.extend(tree_all(&s.src_tree));
        let tgt = target.unwrap_or("");
        let cmd = cmd_resolve(&s.cmd, loader_dir).replace("{target}", tgt);
        se(edges, &name, &deps, &cmd, &s.desc.replace("{target}", tgt));
    };
    let srcgroup = |name: &Option<String>| -> Vec<String> {
        match name {
            Some(g) => resolve_srcs(cfg.srcgroups.get(g).map(|v| v.as_slice()).unwrap_or(&[])),
            None => Vec::new(),
        }
    };

    // 1. leaf steps in the canonical order
    emit_step(&mut edges, step("download"), None);
    emit_step(&mut edges, step("wheel"), None);
    emit_step(&mut edges, step("app"), None);
    emit_step(&mut edges, step("spa"), None);
    // 2. runtimes per target (the make-runtime leaf)
    let mut runtime_stamps = Vec::new();
    for t in targets {
        emit_step(&mut edges, step("runtime"), Some(t));
        runtime_stamps.push(stamp(&format!("runtime-{t}")));
    }

    // 3. components — group 1 then group 2, in file order within each group.
    let mut comp_stamps: Vec<String> = Vec::new();
    for group in [1u8, 2u8] {
        for c in cfg.components.iter().filter(|c| c.group == group) {
            emit_component(&mut edges, c, cfg, targets, &ollama_keys, &active_oses, loader_dir, &srcgroup, &tree_all, &mut comp_stamps);
        }
    }

    // 4. components manifest edge
    se(&mut edges, "components", &comp_stamps, "nix run .#xtask -- components-manifest", "components manifest");

    // 5. bundle per target: stamps + tree(src_tree) + srcs(src)
    let b = &cfg.bundle;
    let mut bundle_stamps = Vec::new();
    for t in targets {
        let mut deps: Vec<String> = b.deps.iter().map(|d| stamp(d)).collect();
        deps.extend(tree_all(&b.src_tree));
        deps.extend(resolve_srcs(&b.src));
        let cmd = cmd_resolve(&b.cmd, loader_dir).replace("{target}", t);
        se(&mut edges, &format!("bundle-{t}"), &deps, &cmd, &b.desc.replace("{target}", t));
        bundle_stamps.push(stamp(&format!("bundle-{t}")));
    }

    // 6. models step
    emit_step(&mut edges, step("models"), None);

    // 7. artifacts (gen edges): bundles + step-stamps + srcs(src) + tree(src_tree)
    for a in &cfg.artifacts {
        let mut deps: Vec<String> = Vec::new();
        if a.deps_bundles {
            deps.extend(bundle_stamps.iter().cloned());
        }
        deps.extend(a.deps_steps.iter().map(|s| stamp(s)));
        deps.extend(resolve_srcs(&a.src));
        deps.extend(tree_all(&a.src_tree));
        edges.push_str(&format!("build {}: gen {}\n  cmd = {}\n  desc = {}\n\n", a.out, deps.join(" "), cmd_resolve(&a.cmd, loader_dir), a.desc));
    }

    // 8. phony aliases (fixed framework set), matching the historical graph.
    for s in ["download", "wheel", "app", "spa", "components", "models"] {
        edges.push_str(&format!("build {s}: phony {}\n", stamp(s)));
    }
    edges.push_str(&format!("build comp-ow-assets: phony {}\n", stamp("comp-ow-assets")));
    edges.push_str(&format!("build comp-ow-assets-sqfs: phony {}\n", stamp("comp-ow-assets-sqfs")));
    edges.push_str(&format!("build comp-ow-assets-dmg: phony {}\n", stamp("comp-ow-assets-dmg")));
    for t in targets {
        edges.push_str(&format!("build comp-runtime-{t}: phony {}\n", stamp(&format!("comp-runtime-{t}"))));
        edges.push_str(&format!("build comp-usbd-{t}: phony {}\n", stamp(&format!("comp-usbd-{t}"))));
    }
    for k in &ollama_keys {
        edges.push_str(&format!("build comp-ollama-{k}: phony {}\n", stamp(&format!("comp-ollama-{k}"))));
    }
    for t in targets {
        edges.push_str(&format!("build runtime-{t}: phony {}\n", stamp(&format!("runtime-{t}"))));
        edges.push_str(&format!("build bundle-{t}: phony {}\n", stamp(&format!("bundle-{t}"))));
    }
    edges.push_str(&format!("build runtimes: phony {}\n", runtime_stamps.join(" ")));
    edges.push_str(&format!("build bundles: phony {}\n", bundle_stamps.join(" ")));
    for a in &cfg.artifacts {
        edges.push_str(&format!("build {}: phony {}\n", a.name, a.out));
    }
    edges.push_str("build all: phony dist/plan-ai-usb.img\n");
    if let Some(a) = cfg.artifacts.iter().find(|a| a.default) {
        edges.push_str(&format!("default {}\n", a.name));
    }

    n.push_str(&edges);
    n
}

#[allow(clippy::too_many_arguments)]
fn emit_component(
    edges: &mut String,
    c: &Component,
    cfg: &Config,
    targets: &[String],
    ollama_keys: &[String],
    active_oses: &[&'static str],
    loader_dir: &str,
    srcgroup: &dyn Fn(&Option<String>) -> Vec<String>,
    tree_all: &dyn Fn(&[String]) -> Vec<String>,
    comp_stamps: &mut Vec<String>,
) {
    // common dep tail: src_stamp + srcgroup + src_extra + src_tree
    let tail = |inst: Option<&str>| -> Vec<String> {
        let mut d = Vec::new();
        if let Some(ss) = &c.src_stamp {
            d.push(stamp(&ss.replace("{target}", inst.unwrap_or(""))));
        }
        d.extend(srcgroup(&c.srcgroup));
        d.extend(c.src_extra.iter().map(|s| dep_resolve(s, loader_dir)));
        d.extend(tree_all(&c.src_tree));
        d
    };
    let comp_cmd = |attr: &str, out: &str| cmd_resolve(&format!("@loader/nix-component.sh {attr} {out}"), loader_dir);

    match c.per.as_str() {
        "shared" if c.builder == "pack" => {
            // ow-assets win-dir pack: explicit cmd/stamp/desc.
            let sname = c.stamp.clone().unwrap();
            let mut deps = Vec::new();
            if let Some(ss) = &c.src_stamp {
                deps.push(stamp(ss));
            }
            deps.extend(srcgroup(&c.srcgroup));
            se(edges, &sname, &deps, &cmd_resolve(c.cmd.as_ref().unwrap(), loader_dir), c.desc.as_ref().unwrap());
            comp_stamps.push(stamp(&sname));
        }
        "shared" if c.builder == "import-build" => {
            // ow-assets sqfs (always) / dmg (only if that OS built). shared_os=linux
            // is the canonical always-present format.
            let os = c.shared_os.as_deref().unwrap_or("linux");
            if os != "linux" && !active_oses.contains(&os) {
                return;
            }
            let sname = c.stamp.clone().unwrap();
            let store = c.store_name.clone().unwrap();
            let src_dir = c.src_dir.clone().unwrap();
            let attr = c.attr.clone().unwrap();
            let out = c.out.clone().unwrap();
            let mut deps = Vec::new();
            if let Some(ss) = &c.src_stamp {
                deps.push(stamp(ss));
            }
            deps.extend(srcgroup(&c.srcgroup));
            let cmd = cmd_resolve(&format!("@loader/import-build-component.sh {store} {src_dir} {attr} {out}"), loader_dir);
            se(edges, &sname, &deps, &cmd, c.desc.as_ref().unwrap());
            comp_stamps.push(stamp(&sname));
        }
        "target" => {
            for t in targets {
                let os = cfg.os_of(t);
                let Some(fmt) = c.format.get(os) else { continue };
                let attr = fmt.attr.replace("{target}", t);
                let out = format!("dist/components/{}-{t}.{}", c.name, fmt.ext);
                let sname = format!("comp-{}-{t}", c.name);
                let cmd = match c.builder.as_str() {
                    "import-build" => {
                        let store = c.store_name.as_ref().unwrap().replace("{target}", t);
                        let src_dir = c.src_dir.as_ref().unwrap().replace("{target}", t);
                        cmd_resolve(&format!("@loader/import-build-component.sh {store} {src_dir} {attr} {out}"), loader_dir)
                    }
                    _ => comp_cmd(&attr, &out),
                };
                se(edges, &sname, &tail(Some(t)), &cmd, &fmt.desc.replace("{target}", t));
                comp_stamps.push(stamp(&sname));
            }
        }
        "flavour" => {
            let keys = if c.flavour.as_deref() == Some("ollama") {
                ollama_keys.to_vec()
            } else {
                cfg.flavour_keys_for(c.flavour.as_deref().unwrap_or(""), targets)
            };
            for k in &keys {
                let os = cfg.os_of(k);
                let Some(fmt) = c.format.get(os) else { continue };
                let attr = fmt.attr.replace("{key}", k);
                let out = format!("dist/components/{}-{k}.{}", c.name, fmt.ext);
                let sname = format!("comp-{}-{k}", c.name);
                se(edges, &sname, &tail(None), &comp_cmd(&attr, &out), &fmt.desc.replace("{key}", k));
                comp_stamps.push(stamp(&sname));
            }
        }
        "shared-os" => {
            for os in ["linux", "mac", "win"] {
                if !active_oses.contains(&os) {
                    continue;
                }
                let Some(fmt) = c.format.get(os) else { continue };
                let sname = fmt.stamp.clone().unwrap();
                let out = fmt.out.clone().unwrap();
                se(edges, &sname, &tail(None), &comp_cmd(&fmt.attr, &out), &fmt.desc);
                comp_stamps.push(stamp(&sname));
            }
        }
        other => panic!("loader.toml component {}: unsupported per={other}", c.name),
    }
}

/// Render the Makefile from `loader.toml [make]`. Generic preamble (TARGET/PLATFORMS
/// knobs keyed by `[targets].select_env`, the nix-develop guard, the XTASK wrapper) +
/// one rule per `[[make.target]]` + a `help` target. Committed so `make` can trigger
/// xtask, but regenerated by `xtask gen-makefile` (the source of truth is loader.toml).
pub fn render_makefile(cfg: &Config) -> Result<String, String> {
    let make = cfg.make.as_ref().ok_or("loader.toml has no [make] section")?;
    let select_env = cfg.targets.select_env.as_deref().unwrap_or("PLANAI_PLATFORMS");
    let mut m = String::new();
    m.push_str(&format!(
        "# {} — pipeline entrypoints. GENERATED by `xtask gen-makefile` from loader.toml — do not edit by hand.\n",
        cfg.manifest.product
    ));
    m.push_str("# Run inside `nix develop` (the build leg). TARGET defaults to the host.\n");
    m.push_str("TARGET ?=\n");
    m.push_str(&format!("ifneq ($(TARGET),)\nexport {select_env} ?= $(TARGET)\nendif\n\n"));
    m.push_str("# Subset the build to specific platforms instead of the lock's full set, e.g.\n");
    m.push_str("#   make image PLATFORMS=linux-x64\n");
    m.push_str(&format!("PLATFORMS ?= $(TARGET)\nexport {select_env} = $(PLATFORMS)\n\n"));

    // nix-develop guard (every target except the excluded ones needs the devshell).
    let excl = make.guard_exclude.join(" ");
    m.push_str("# --- nix develop guard ------------------------------------------------------\n");
    m.push_str(&format!("GUARDED := $(filter-out {excl},$(or $(MAKECMDGOALS),{}))\n", make.default_goal));
    m.push_str("ifneq ($(GUARDED),)\n");
    m.push_str(&format!("ifndef {}\n", make.devshell_var));
    m.push_str("$(error not in the devshell — run 'nix develop' first, or 'nix develop --command make $(MAKECMDGOALS)')\n");
    m.push_str("endif\nendif\n\n");

    // XTASK wrapper — '#' starts a Make comment, so escape it in the flake attr.
    m.push_str("# Artifact builds go through ninja via the xtask orchestrator (built offline by nix).\n");
    m.push_str(&format!("XTASK := {}\n\n", make.xtask_cmd.replace('#', "\\#")));

    // .PHONY for every phony target + help.
    let mut phony: Vec<&str> = make.targets.iter().filter(|t| t.phony).map(|t| t.name.as_str()).collect();
    phony.push("help");
    m.push_str(&format!(".PHONY: {}\n\n", phony.join(" ")));

    for t in &make.targets {
        let deps = if t.deps.is_empty() { String::new() } else { format!(" {}", t.deps.join(" ")) };
        let help = if t.help.is_empty() { String::new() } else { format!(" ## {}", t.help) };
        m.push_str(&format!("{}:{deps}{help}\n", t.name));
        for line in &t.recipe {
            m.push('\t');
            m.push_str(line);
            m.push('\n');
        }
        m.push('\n');
    }

    // help: list documented targets (## comments), like the hand-written Makefile.
    m.push_str("help: ## list targets\n");
    m.push_str("\t@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | sort | \\\n");
    m.push_str("\t  awk 'BEGIN{FS=\":.*?## \"}{printf \"  \\033[36m%-14s\\033[0m %s\\n\", $$1, $$2}'\n");
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The project repo root (this crate lives at third_party/loader/crates/loader-engine).
    fn repo_root() -> Option<PathBuf> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..").canonicalize().ok()?;
        root.join("loader.toml").is_file().then_some(root)
    }

    #[test]
    fn parses_loader_toml() {
        let Some(root) = repo_root() else {
            eprintln!("skipping: project loader.toml not present (standalone submodule checkout)");
            return;
        };
        let txt = std::fs::read_to_string(root.join("loader.toml")).unwrap();
        let cfg = parse(&txt).expect("loader.toml parses");
        assert_eq!(cfg.manifest.product, "plan-ai-usb");
        assert!(cfg.components.iter().any(|c| c.name == "ollama"));
    }

    /// The feature tagging derived from `[[component]].feature` reproduces the historical
    /// basename-heuristic exactly for every real component output (and tags the LLM
    /// engine + the launchers as core/None) — proving the authoritative build-step
    /// declaration is equivalent to, and replaces, the old guess.
    #[test]
    fn derived_feature_tagging_matches_historical() {
        let Some(root) = repo_root() else {
            eprintln!("skipping: project tree not present");
            return;
        };
        let cfg = parse(&std::fs::read_to_string(root.join("loader.toml")).unwrap()).unwrap();
        let table = cfg.feature_classify_table();
        // mimic loader_manifest::ClassifyTable::classify_feature (basename prefix match)
        let feat = |rel: &str| -> Option<String> {
            let p = rel.to_ascii_lowercase();
            let base = p.rsplit('/').next().unwrap_or(&p).to_string();
            table
                .iter()
                .find(|(_, prefixes)| prefixes.iter().any(|pre| base.starts_with(&pre.to_ascii_lowercase())))
                .map(|(f, _)| f.clone())
        };
        let cases = [
            ("components/linux-x64/ow-assets.squashfs", Some("openwebui")),
            ("components/mac-arm64/ow-assets.dmg", Some("openwebui")),
            ("components/linux-x64/runtime-linux-x64.squashfs", Some("openwebui")),
            ("components/linux-x64/llamacpp-linux-amd64.squashfs", Some("llamacpp")),
            ("components/linux-x64/hermes-linux-x64.squashfs", Some("hermes")),
            ("components/linux-x64/hermes-webui.squashfs", Some("hermes")),
            ("components/win-x64/hermes-win-x64.zip", Some("hermes")),
            // core (no feature gate): the LLM engine, the daemon, launchers, markers
            ("components/linux-x64/ollama-linux-amd64.squashfs", None),
            ("components/linux-x64/usbd-linux-x64.squashfs", None),
            ("components/linux-x64/manifest.json", None),
            ("plan-ai.exe", None),
        ];
        for (path, want) in cases {
            assert_eq!(feat(path).as_deref(), want, "feature tag for {path}");
        }
    }

    /// Structural soundness of the rendered graph (robust — no dependency on a
    /// freshly-regenerated, gitignored build.ninja): the engine renders the project's
    /// loader.toml for linux-x64 without panic, into a well-formed ninja graph — the
    /// rules + a `default` line, the expected leaf/component/bundle edges, no duplicate
    /// `build` outputs, and every `stamp`-edge dependency has a producing edge.
    #[test]
    fn renders_sound_graph() {
        let Some(root) = repo_root() else {
            eprintln!("skipping: project tree not present");
            return;
        };
        let cfg = parse(&std::fs::read_to_string(root.join("loader.toml")).unwrap()).unwrap();
        let (srcs, tree) = fs_src_fns(root.clone());
        let f = SrcFns { srcs: &srcs, tree: &tree };
        let g = render(&cfg, &["linux-x64".to_string()], "third_party/loader/scripts", &f);

        assert!(g.contains("rule stamp") && g.contains("rule gen"), "missing rules");
        assert!(g.trim_end().ends_with("default image"), "missing/!last `default image`");
        // representative edges from the linux-x64 graph
        for needle in [
            "build dist/.ninja/download.stamp:",
            "build dist/.ninja/comp-ollama-linux-amd64.stamp:",
            "build dist/.ninja/comp-runtime-linux-x64.stamp:",
            "build dist/.ninja/components.stamp:",
            "build dist/.ninja/bundle-linux-x64.stamp:",
            "build dist/plan-ai-usb.img: gen",
            "third_party/loader/scripts/nix-component.sh", // loader-script prefix applied
            "./scripts/fetch-vendor.sh",                   // project script stays ./scripts
        ] {
            assert!(g.contains(needle), "rendered graph missing: {needle}");
        }
        // collect declared outputs; no duplicates, and every stamp dep is produced.
        let mut outputs = std::collections::HashSet::new();
        for line in g.lines().filter(|l| l.starts_with("build ")) {
            let head = line.trim_start_matches("build ").split(':').next().unwrap();
            for out in head.split_whitespace() {
                assert!(outputs.insert(out.to_string()), "duplicate build output: {out}");
            }
        }
        for line in g.lines().filter(|l| l.starts_with("build ") && l.contains(": stamp")) {
            let after = line.split(": stamp").nth(1).unwrap_or("");
            for dep in after.split_whitespace().filter(|d| d.ends_with(".stamp")) {
                assert!(outputs.contains(dep), "stamp dep with no producer: {dep}");
            }
        }
    }
}
