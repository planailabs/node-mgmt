//! Resource + portable paths for the supervised services (ported from
//! app/main/paths.js). The launcher exports PLANAI_RESOURCES (the prepared
//! component tree: runtime/, ollama/, ow-assets/) and, optionally,
//! PLANAI_PORTABLE_ROOT (the writable USB-side root holding models/ + data/).

use std::path::PathBuf;

pub fn resources_root() -> PathBuf {
    std::env::var_os("PLANAI_RESOURCES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dist"))
}

pub fn portable_root() -> PathBuf {
    if let Some(p) = std::env::var_os("PLANAI_PORTABLE_ROOT") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn ensure_dir(p: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn models_dir() -> PathBuf {
    ensure_dir(portable_root().join("models"))
}
pub fn data_dir() -> PathBuf {
    ensure_dir(portable_root().join("data"))
}
pub fn ow_assets() -> PathBuf {
    resources_root().join("ow-assets")
}

/// The mounted llama.cpp component's llama-server (optional "llamacpp"
/// feature). Upstream archives differ: linux/mac pack build/bin/, win is flat.
pub fn llamacpp_server() -> PathBuf {
    let root = resources_root().join("llamacpp");
    let name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    for c in [root.join("build").join("bin").join(name), root.join("bin").join(name)] {
        if c.exists() {
            return c;
        }
    }
    root.join(name)
}

/// The hermes component's bundled python (optional "hermes" feature).
pub fn hermes_python() -> PathBuf {
    let root = resources_root().join("hermes").join("python");
    if cfg!(windows) {
        root.join("python.exe")
    } else {
        root.join("bin").join("python3")
    }
}

/// The ollama binary for this OS, from <resources>/ollama/ (tolerating a bin/ nest).
pub fn ollama_binary() -> PathBuf {
    let dir = resources_root().join("ollama");
    let name = if cfg!(windows) { "ollama.exe" } else { "ollama" };
    for c in [dir.join(name), dir.join("bin").join(name)] {
        if c.exists() {
            return c;
        }
    }
    dir.join(name)
}

fn runtime_root() -> PathBuf {
    resources_root().join("runtime")
}

/// The python interpreter — a host venv (runtime/venv) or a relocatable
/// python-build-standalone tree (runtime/python).
pub fn venv_python() -> PathBuf {
    let rt = runtime_root();
    let mut cands: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        cands.push(rt.join("venv").join("Scripts").join("python.exe"));
        cands.push(rt.join("python").join("python.exe"));
    } else {
        cands.push(rt.join("venv").join("bin").join("python"));
        for base in ["python", "venv"] {
            let bin = rt.join(base).join("bin");
            cands.push(bin.join("python3"));
            if let Ok(rd) = std::fs::read_dir(&bin) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let is_py = n == "python3"
                        || (n.starts_with("python3.") && n["python3.".len()..].chars().all(|c| c.is_ascii_digit()));
                    if is_py {
                        cands.push(e.path());
                    }
                }
            }
        }
    }
    cands.iter().find(|c| c.exists()).cloned()
        .or_else(|| cands.into_iter().next())
        .unwrap_or_else(|| rt.join("python").join("bin").join("python3"))
}

fn site_packages_roots() -> Vec<PathBuf> {
    let rt = runtime_root();
    let mut roots = Vec::new();
    for base in ["venv", "python"] {
        roots.push(rt.join(base).join("Lib").join("site-packages")); // windows
        if let Ok(rd) = std::fs::read_dir(rt.join(base).join("lib")) {
            for e in rd.flatten() {
                roots.push(e.path().join("site-packages"));
            }
        }
    }
    roots
}

/// Open-WebUI's installed frontend dir (env.py's default is wrong for a wheel).
pub fn ow_frontend_dir() -> Option<PathBuf> {
    for sp in site_packages_roots() {
        let c = sp.join("open_webui").join("frontend");
        if c.join("index.html").exists() {
            return Some(c);
        }
    }
    None
}
