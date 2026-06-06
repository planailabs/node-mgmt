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
