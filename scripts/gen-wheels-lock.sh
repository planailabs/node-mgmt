#!/usr/bin/env bash
# Produce runtime/wheels-<target>.lock.json: every wheel from runtime/uv.lock that
# is compatible with <target> (default linux-x64 / cp312), each with an SRI hash
# (prefetching the few the pytorch index serves without one). flake.nix turns this
# into fetchurl FODs (a wheelhouse) that a vanilla derivation installs offline
# into the portable pbs tree (scripts/make-runtime.sh). This is the wheels-FOD
# realization of "uv2nix == wheels-fod": uv resolves, Nix fetches, no fixup.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need python3; need nix

# default: every distributable target (mac-x64 dropped — arm64-only wheels)
TARGETS=("$@"); [ ${#TARGETS[@]} -eq 0 ] && TARGETS=(linux-x64 win-x64 mac-arm64)
[ -f "$REPO_ROOT/runtime/uv.lock" ] || die "runtime/uv.lock missing — run: cd runtime && uv lock"

for TARGET in "${TARGETS[@]}"; do
OUT="$REPO_ROOT/runtime/wheels-$TARGET.lock.json"
log "selecting $TARGET wheels from runtime/uv.lock (+ normalising hashes to SRI)"
python3 - "$TARGET" "$REPO_ROOT/runtime/uv.lock" "$OUT" <<'PY'
import sys, json, re, subprocess, tomllib
from urllib.parse import unquote
target, lockpath, out = sys.argv[1], sys.argv[2], sys.argv[3]
lock = tomllib.load(open(lockpath, "rb"))

# python tag + platform-tag matchers per target (cp312)
PYTAG = "cp312"
PLAT = {
    "linux-x64": [r"manylinux.*x86_64", r"linux_x86_64", r"musllinux.*x86_64"],
    "win-x64":   [r"win_amd64"],
    "mac-arm64": [r"macosx.*arm64", r"macosx.*universal2"],
}[target]
plat_re = re.compile("|".join(PLAT))

def compatible(fname):
    # pure-python, any-platform wheels
    if re.search(r"-(py3|py2\.py3|cp3[0-9]+)-none-any\.whl$", fname):
        return True
    # platform-specific wheels for this OS/arch. Accept cp312 / abi3 ABI tags AND
    # the generic py3/py2.py3 python tag — some packages ship a pure-python wheel
    # whose ONLY platform variant bundles a native lib (e.g. soundfile ships
    # soundfile-*-py2.py3-none-manylinux_*.whl with libsndfile.so inside; the
    # plain none-any wheel has no lib and crashes off NixOS). uv then prefers the
    # platform wheel for --python-platform.
    if plat_re.search(fname) and (
        re.search(rf"-({PYTAG}|cp3[0-9]+-abi3|abi3|py3|py2\.py3)-", fname)
        or f"-{PYTAG}-{PYTAG}-" in fname
    ):
        return True
    return False

def to_sri(url, recorded_hex):
    if recorded_hex:
        sri = subprocess.check_output(
            ["nix", "hash", "convert", "--hash-algo", "sha256", "--to", "sri", recorded_hex],
            text=True).strip()
        return sri
    # pytorch index serves some files without a hash — prefetch to get it
    j = json.loads(subprocess.check_output(["nix", "store", "prefetch-file", "--json", url], text=True))
    return j["hash"]

wheels, seen = [], set()
for pkg in lock.get("package", []):
    for w in pkg.get("wheels", []):
        url = w["url"]; fname = unquote(url.rsplit("/", 1)[-1])  # decode %2Bcpu -> +cpu for find-links
        if not compatible(fname) or fname in seen:
            continue
        seen.add(fname)
        hexh = (w.get("hash") or "").removeprefix("sha256:")
        wheels.append({"name": fname, "url": url, "hash": to_sri(url, hexh)})

json.dump({"target": target, "wheels": sorted(wheels, key=lambda x: x["name"])}, open(out, "w"), indent=2)
print(f"{len(wheels)} wheels -> {out}")
PY
done
log "wheel locks written for: ${TARGETS[*]}"
