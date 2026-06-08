# Layer 3 (wheels-FOD): the Open-WebUI python runtime, built in nix but PORTABLE.
#
# uv resolves (runtime/uv.lock); scripts/gen-wheels-lock.sh selects the target's
# wheels + their SRI hashes -> wheels-<target>.lock.json. Here each wheel is a
# fixed-output derivation (a "wheelhouse"), and a VANILLA derivation (no stdenv,
# no fixup/patchelf) installs them offline into the python-build-standalone tree
# with `uv pip install --target --python-platform --no-index`. No interpreter is
# executed (works on NixOS) and the manylinux .so files are left untouched, so the
# result runs OUTSIDE the nix store (copied out by scripts/make-runtime.sh).
{ pkgs, lib, system, pyVersion, triple, pbsArchive, wheelsLock }:
let
  wheelhouse = pkgs.linkFarm "wheelhouse" (map
    (w: { name = w.name; path = pkgs.fetchurl { inherit (w) url hash; }; })
    wheelsLock.wheels);
  # MSVC C++ redistributable runtime (MSVCP140*.dll, concrt140.dll, …) for the
  # Windows runtime. python-build-standalone bundles only the C runtime
  # (vcruntime140*.dll), but torch's torch_cpu/torch_python.dll (pulled in by
  # sentence-transformers → open-webui embeddings) link MSVCP140.dll +
  # MSVCP140_ATOMIC_WAIT.dll, so without these open-webui dies at `import torch`
  # with WinError 126. A portable kiosk can't assume the VC++ redist is installed
  # → bundle these redistributable DLLs into the python root. The `msvc-runtime`
  # PyPI wheel just ships the redist DLLs (version-agnostic; the cp311 tag is
  # irrelevant — we take only the DLLs).
  msvcRuntimeWheel = pkgs.fetchurl {
    url = "https://files.pythonhosted.org/packages/ce/92/5a10262c2a489d5854f96d69e287923d6f720c4935dd26634deb7a5426e9/msvc_runtime-14.44.35112-cp311-cp311-win_amd64.whl";
    hash = "sha256-q6f75xiX0l7VP7t/OR6fUCiTeKipriGLoYUwxmNEg5E=";
  };
in
derivation {
  inherit system;
  name = "plan-ai-runtime-${wheelsLock.target}";
  builder = "${pkgs.bash}/bin/bash";
  args = [ "-c" ''
    # python3 is only for uv's interpreter discovery; --python-platform/--python-version
    # drive wheel selection and --target installs into the pbs tree (no wheel is executed).
    export PATH="${lib.makeBinPath (with pkgs; [ coreutils gnutar gzip uv python3 ])}"
    export HOME="$TMPDIR" UV_NO_INDEX=1 UV_PYTHON_DOWNLOADS=never UV_PYTHON_PREFERENCE=only-system
    # macOS: onnxruntime (via chromadb) only ships macosx_14_0 wheels, so resolve
    # against that deployment target or resolution is unsatisfiable.
    ${lib.optionalString (lib.hasSuffix "apple-darwin" triple) ''export MACOSX_DEPLOYMENT_TARGET=14.0''}
    mkdir -p "$out"
    tar -xzf "$pbsArchive" -C "$out" --strip-components=1
    # Windows: bundle the MSVC C++ runtime into the python root (next to
    # python.exe / vcruntime140.dll, on the DLL search path) so torch loads.
    ${lib.optionalString (lib.hasInfix "windows" triple) ''
      python3 -m zipfile -e "${msvcRuntimeWheel}" "$TMPDIR/msvcrt"
      cp "$TMPDIR"/msvcrt/msvc_runtime-*.data/data/*.dll "$out/"
    ''}
    # site-packages location differs by OS layout: Windows pbs uses Lib/site-packages
    # (python.exe at root); unix uses lib/python<X.Y>/site-packages. Resolve the real
    # path so uv --target installs where the interpreter will actually look (a literal
    # unexpanded glob would create a bogus "python*" dir python never imports from).
    ${if lib.hasInfix "windows" triple
      then ''sp="$out/Lib/site-packages"''
      else ''sp="$(ls -d "$out"/lib/python*/site-packages)"''}
    mkdir -p "$sp"
    uv pip install \
      --target "$sp" \
      --python-platform "${triple}" \
      --python-version "${pyVersion}" \
      --no-index --find-links "${wheelhouse}" --offline \
      open-webui
    [ -f "$sp/open_webui/main.py" ] || { echo "open_webui not installed into $sp" >&2; exit 1; }
    find "$out" -name '__pycache__' -type d -prune -exec rm -rf {} + || true
  '' ];
  pbsArchive = pbsArchive;
}
