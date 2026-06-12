fn main() {
    // Git commit the crate was built from (reported in metrics and heartbeats). Prefer
    // GIT_SHA from the environment (CI / nix), fall back to `git rev-parse
    // HEAD` when building from a checkout. "unknown" when neither works
    // (e.g. building from a source tarball). Mirrors mac-mgmt-agent/build.rs.
    let sha = std::env::var("GIT_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().trim_end_matches("-dirty").to_string())
        .or_else(|| {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if sha.is_empty() {
                return None;
            }
            Some(sha)
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo::rustc-env=GIT_SHA={sha}");
    println!("cargo::rerun-if-env-changed=GIT_SHA");
    println!("cargo::rerun-if-changed=../.git/HEAD");
    println!("cargo::rerun-if-changed=../.git/index");
}
