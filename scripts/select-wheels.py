#!/usr/bin/env python3
"""Select the wheels compatible with a target from a uv.lock, as SRI-hashed JSON.

Shared by gen-wheels-lock.sh (the open-webui runtime, runtime/uv.lock) and
gen-hermes-wheels-lock.sh (the hermes component, the vendored hermes uv.lock).
The output (wheels-<target>.lock.json) feeds the flake's wheelhouse FODs that a
vanilla derivation installs offline into the portable pbs tree.

usage: select-wheels.py <target> <uv.lock> <out.json> [--py-tag cp312]
                        [--only <names-file>]

--only limits the selection to the newline-separated package names in
<names-file> (normalized PEP 503). Without it every wheel in the lock that fits
the target is selected (the open-webui behavior, where the lock IS the closure).
"""
import sys, json, re, subprocess, tomllib
from urllib.parse import unquote

target, lockpath, out = sys.argv[1], sys.argv[2], sys.argv[3]
pytag = "cp312"
only = None
args = sys.argv[4:]
while args:
    a = args.pop(0)
    if a == "--py-tag":
        pytag = args.pop(0)
    elif a == "--only":
        names = open(args.pop(0)).read().split()
        only = {re.sub(r"[-_.]+", "-", n).lower() for n in names}
    else:
        sys.exit(f"unknown arg: {a}")

lock = tomllib.load(open(lockpath, "rb"))

PLAT = {
    "linux-x64": [r"manylinux.*x86_64", r"linux_x86_64", r"musllinux.*x86_64"],
    "linux-arm64": [r"manylinux.*aarch64", r"linux_aarch64", r"musllinux.*aarch64"],
    "win-x64":   [r"win_amd64"],
    "mac-arm64": [r"macosx.*arm64", r"macosx.*universal2"],
}[target]
plat_re = re.compile("|".join(PLAT))

def compatible(fname):
    # pure-python, any-platform wheels
    if re.search(r"-(py3|py2\.py3|cp3[0-9]+)-none-any\.whl$", fname):
        return True
    # platform-specific wheels for this OS/arch. Accept cp-matching / abi3 ABI
    # tags AND the generic py3/py2.py3 python tag — some packages ship a pure
    # wheel whose ONLY platform variant bundles a native lib (e.g. soundfile's
    # py2.py3-none-manylinux wheel carries libsndfile.so; the none-any wheel
    # has no lib and crashes off NixOS). uv then prefers the platform wheel
    # for --python-platform.
    if plat_re.search(fname) and (
        re.search(rf"-({pytag}|cp3[0-9]+-abi3|abi3|py3|py2\.py3)-", fname)
        or f"-{pytag}-{pytag}-" in fname
    ):
        return True
    return False

def to_sri(url, recorded_hex):
    if recorded_hex:
        return subprocess.check_output(
            ["nix", "hash", "convert", "--hash-algo", "sha256", "--to", "sri", recorded_hex],
            text=True).strip()
    # some indexes (pytorch) serve files without a hash — prefetch to get one
    j = json.loads(subprocess.check_output(["nix", "store", "prefetch-file", "--json", url], text=True))
    return j["hash"]

wheels, seen = [], set()
for pkg in lock.get("package", []):
    if only is not None:
        name = re.sub(r"[-_.]+", "-", pkg.get("name", "")).lower()
        if name not in only:
            continue
    for w in pkg.get("wheels", []):
        url = w["url"]; fname = unquote(url.rsplit("/", 1)[-1])  # decode %2Bcpu for find-links
        if not compatible(fname) or fname in seen:
            continue
        seen.add(fname)
        hexh = (w.get("hash") or "").removeprefix("sha256:")
        wheels.append({"name": fname, "url": url, "hash": to_sri(url, hexh)})

json.dump({"target": target, "wheels": sorted(wheels, key=lambda x: x["name"])}, open(out, "w"), indent=2)
print(f"{len(wheels)} wheels -> {out}")
