# ARCHIVED — from-source Open-WebUI wheel build (NOT wired into the shipped build).
#
# Why archived: the shipped runtime installs open_webui from a PINNED PyPI wheel
# (FOD, runtime/wheels-*.lock.json), which is already hermetic. This builds the
# same wheel FROM SOURCE in nix instead — kept for if we ever need to patch
# open-webui or drop the PyPI dependency. Verified working (matches the PyPI wheel
# within ~119 bytes — zip metadata).
#
# To revive: paste these `let` bindings back into nix/builds.nix (and re-`inherit
# ow-frontend wheel;`), restore archive/from-source-openwebui/fetch-ow-frontend.sh
# to scripts/, and re-add the ninja edges in xtask render_ninja:
#   ow-inputs (fetch-ow-frontend.sh) -> ow-frontend (-A ow-frontend) -> wheel (-A wheel)
# then point runtime.nix at the local wheel store path instead of the PyPI FOD.
#
# Build (with the store-import foundation populated): nix-build --impure -A wheel
{ pkgs, stores }:
let
  owSrc = stores.ow-src or (throw "ow-src not imported — run fetch-ow-frontend.sh");

  # Open-WebUI frontend, built OFFLINE from store-imported inputs (npm ci +
  # pyodide:fetch happen outside; their results are imported). Run `vite build`
  # directly — `npm run build` would re-run the networked pyodide:fetch.
  ow-frontend = pkgs.stdenv.mkDerivation {
    name = "ow-frontend";
    nativeBuildInputs = [ pkgs.nodejs_22 ];
    dontUnpack = true;
    buildPhase = ''
      runHook preBuild
      export HOME="$TMPDIR"
      # COPY (not symlink) node_modules: vite's config loader resolves symlinks via
      # esbuild, which chases a symlinked node_modules to its real store-path name and
      # then can't find sibling pkgs (kleur, rollup). `cp -r` keeps exec bits (esbuild
      # binary; --no-preserve=mode would strip them -> EACCES); chmod +w after.
      cp -r ${owSrc} src
      chmod -R u+w src && cd src
      cp -r ${stores.ow-node-modules} node_modules && chmod -R u+w node_modules
      mkdir -p static && cp -r ${stores.ow-pyodide} static/pyodide && chmod -R u+w static/pyodide
      # call node directly: the .bin shims use `#!/usr/bin/env node`, absent in the sandbox
      node node_modules/vite/bin/vite.js build
      runHook postBuild
    '';
    installPhase = "cp -r build $out";
  };

  # python env carrying the wheel build backend so `python -m build --no-isolation`
  # needs no network (uv/hatchling would otherwise fetch the backend from PyPI).
  buildPython = pkgs.python312.withPackages (ps: [ ps.hatchling ps.build ]);

  # The platform-agnostic open_webui wheel, OFFLINE. hatchling force-includes
  # build/ -> open_webui/frontend; we supply build/ from ow-frontend instead of
  # letting the custom hatch hook run `npm install && npm run build` (network).
  wheel = pkgs.stdenv.mkDerivation {
    name = "open-webui-wheel";
    nativeBuildInputs = [ buildPython ];
    dontUnpack = true;
    buildPhase = ''
      runHook preBuild
      export HOME="$TMPDIR"
      cp -r ${owSrc} src
      chmod -R u+w src && cd src
      cp -r ${ow-frontend} build && chmod -R u+w build
      # strip the custom hatch hook (it npm-installs + builds the frontend = network)
      awk '/^\[tool\.hatch\.build\.hooks\.custom\]/{skip=1;next} /^\[/{skip=0} !skip{print}' \
        pyproject.toml > pyproject.toml.tmp && mv pyproject.toml.tmp pyproject.toml
      python -m build --wheel --no-isolation --outdir out-wheel
      runHook postBuild
    '';
    installPhase = "mkdir -p $out; cp out-wheel/*.whl $out/";
  };
in
{ inherit ow-frontend wheel; }
