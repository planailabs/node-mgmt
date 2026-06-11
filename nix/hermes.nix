# The hermes-agent component (optional "hermes" feature), built in nix but
# PORTABLE — the same wheels-FOD pattern as the open-webui runtime
# (nix/runtime.nix): uv resolves (the vendored hermes uv.lock →
# hermes/wheels-<target>.lock.json), nix fetches each wheel as a FOD, and a
# VANILLA derivation installs them offline into a python-build-standalone tree
# with `uv pip install --target` — no interpreter executed, no fixup/patchelf,
# so the result runs OUTSIDE the nix store on any machine.
#
# We lean on the upstream repo's own nix packaging knowledge (nix/hermes-agent.nix,
# nix/web.nix) but NOT its derivations: those produce store-dependent wrappers
# around a nixpkgs python. Instead:
#   - the hermes-agent wheel is built from the vendored source (pure-python
#     setuptools), patched so lazy dep installs go to an overlay venv at
#     $HERMES_HOME/venv when the bundled venv is read-only (a mounted
#     squashfs/dmg) — hermes/patches/, adapted from nixpkgs' hermes-agent.
#   - skills/ plugins/ locales/ are copied INTO site-packages (upstream wheels
#     don't carry them; locales has no env-var override at this tag, the other
#     two double as HERMES_BUNDLED_* targets for the launcher).
#   - the web dashboard SPA (web/) is vite-built (buildNpmPackage, upstream's
#     own npmDepsHash) into share/web_dist — served by `hermes dashboard`
#     (HERMES_WEB_DIST), pure static files, no node at runtime.
{ pkgs, lib, system, pyVersion, triple, pbsArchive, wheelsLock, hermesSrc, hermesTag }:
let
  wheelhouse = pkgs.linkFarm "hermes-wheelhouse" (map
    (w: { name = w.name; path = pkgs.fetchurl { inherit (w) url hash; }; })
    wheelsLock.wheels);

  # Vendored source + the lazy-deps overlay-venv patches (01 = nixpkgs', 02 =
  # our read-only-venv predicate on top).
  patchedSrc = pkgs.applyPatches {
    name = "hermes-agent-src-${hermesTag}";
    src = hermesSrc;
    patches = [
      ../hermes/patches/01-nix-venv-lazy-deps.patch
      ../hermes/patches/02-overlay-venv-readonly.patch
      # upstream drift at this tag: pyproject pins aiohttp==3.13.3 but its own
      # uv.lock (whose wheels fill our wheelhouse) resolved 3.13.4.
      ../hermes/patches/03-pyproject-aiohttp-lock-drift.patch
    ];
  };

  # The hermes-agent wheel: pure python (setuptools build backend, no ext
  # modules) — built ONCE, platform independent, fed to uv via find-links.
  buildPython = pkgs.python312.withPackages (ps: [ ps.build ps.setuptools ps.wheel ]);
  hermesWheel = pkgs.runCommand "hermes-agent-wheel-${hermesTag}" { nativeBuildInputs = [ buildPython ]; } ''
    cp -r ${patchedSrc} src && chmod -R u+w src && cd src
    python3 -m build --wheel --no-isolation --outdir "$out"
    ls "$out"/hermes_agent-*.whl >/dev/null
  '';

  # The web dashboard SPA, vite-built from the vendored web/ workspace with
  # upstream's own pinned npmDepsHash (their nix/web.nix at this tag). Output is
  # static js/css/html — store-independent content by construction.
  webNpmDepsHash = "sha256-HWB1piIPglTXbzQHXFYHLgVZIbDb60esupXSQGa1+lI=";
  webDist = pkgs.buildNpmPackage {
    pname = "hermes-web";
    version = hermesTag;
    src = "${hermesSrc}/web";
    npmDeps = pkgs.fetchNpmDeps {
      src = "${hermesSrc}/web";
      hash = webNpmDepsHash;
    };
    # npmConfigHook diffs the source lockfile against the cache's — normalise to
    # exactly one trailing newline like upstream's nix/lib.nix patchPhase.
    postPatch = ''
      ${pkgs.gnused}/bin/sed -i -e :a -e '/^\n*$/{$d;N;ba' -e '}' package-lock.json
    '';
    doCheck = false;
    # package.json's build script writes to ../hermes_cli/web_dist (the monorepo
    # layout); override the outDir like upstream's nix/web.nix.
    buildPhase = ''
      runHook preBuild
      npx tsc -b
      npx vite build --outDir dist
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  };

  msvcRuntimeWheel = (import ./msvc-runtime.nix { inherit pkgs; }).wheel;
in
derivation {
  inherit system;
  name = "plan-ai-hermes-${wheelsLock.target}";
  builder = "${pkgs.bash}/bin/bash";
  args = [ "-c" ''
    export PATH="${lib.makeBinPath (with pkgs; [ coreutils findutils gnutar gzip uv python3 ])}"
    export HOME="$TMPDIR" UV_NO_INDEX=1 UV_PYTHON_DOWNLOADS=never UV_PYTHON_PREFERENCE=only-system
    ${lib.optionalString (lib.hasSuffix "apple-darwin" triple) ''export MACOSX_DEPLOYMENT_TARGET=14.0''}
    mkdir -p "$out/python" "$out/share"
    tar -xzf "$pbsArchive" -C "$out/python" --strip-components=1
    ${lib.optionalString (lib.hasInfix "windows" triple) ''
      python3 -m zipfile -e "${msvcRuntimeWheel}" "$TMPDIR/msvcrt"
      cp "$TMPDIR"/msvcrt/msvc_runtime-*.data/data/*.dll "$out/python/"
    ''}
    ${if lib.hasInfix "windows" triple
      then ''sp="$out/python/Lib/site-packages"''
      else ''sp="$(ls -d "$out"/python/lib/python*/site-packages)"''}
    mkdir -p "$sp"
    uv pip install \
      --target "$sp" \
      --python-platform "${triple}" \
      --python-version "${pyVersion}" \
      --no-index --find-links "${wheelhouse}" --find-links "${hermesWheel}" --offline \
      "hermes-agent[all,messaging]"
    [ -f "$sp/hermes_cli/main.py" ] || { echo "hermes_cli not installed into $sp" >&2; exit 1; }

    # Bundled data the wheel doesn't carry: locales/ MUST sit at
    # site-packages/locales (agent/i18n.py has no env override at this tag);
    # skills/ + plugins/ land there too (the site-packages fallback) and the
    # launcher additionally points HERMES_BUNDLED_SKILLS/PLUGINS at them.
    cp -r ${patchedSrc}/locales "$sp/locales"
    cp -r ${patchedSrc}/skills "$sp/skills"
    chmod -R u+w "$sp/plugins" 2>/dev/null || true
    cp -r ${patchedSrc}/plugins/. "$sp/plugins/"

    # The dashboard SPA (HERMES_WEB_DIST).
    cp -r ${webDist} "$out/share/web_dist"

    echo '{ "version": "${hermesTag}", "entry": "hermes_cli.main" }' > "$out/hermes.json"
    find "$out" -name '__pycache__' -type d -prune -exec rm -rf {} + || true
  '' ];
  pbsArchive = pbsArchive;
}
