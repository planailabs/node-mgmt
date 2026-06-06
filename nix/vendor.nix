# Layers 1 & 2: vendored downloads as fixed-output derivations, and the no-fixup
# ollama component repack. Outputs are portable archives copied out of the store.
{ pkgs, lib, system, vendorLock }:
let
  fetch = e: pkgs.fetchurl { inherit (e) url sha256; };

  # Layer 1 — every upstream archive (ollama flavours, python-build-standalone,
  # open-webui source) keyed by the sha256 in vendor.lock.json. FODs: cached,
  # content-addressed, NO fixup, binaries byte-identical.
  vendor = pkgs.linkFarm "plan-ai-vendor" (
    (map (a: { name = "ollama/${vendorLock.ollama.tag}/${a.name}"; path = fetch a; }) vendorLock.ollama.assets)
    ++ (map (p: { name = "pbs/${p.target}.tar.gz"; path = fetch p; }) vendorLock.pbs.files)
    ++ [ { name = "open-webui/${vendorLock.openwebui.tag}/source.tar.gz"; path = fetch vendorLock.openwebui; } ]
  );

  # Layer 2 — normalise each ollama FOD to a uniform .tar.gz. VANILLA derivation
  # (builtins.derivation): no stdenv, no setup hooks, so no fixup/patchelf/strip
  # can touch the generic ollama binaries.
  ollamaKeyOf = name: lib.pipe name [
    (lib.removePrefix "ollama-")
    (lib.removeSuffix ".tar.zst") (lib.removeSuffix ".tgz") (lib.removeSuffix ".zip")
  ];
  repackOllama = a: derivation {
    inherit system;
    name = "ollama-${ollamaKeyOf a.name}.tar.gz";
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      export PATH="${lib.makeBinPath (with pkgs; [ coreutils gnutar zstd pigz unzip ])}"
      mkdir x
      case "$assetName" in
        *.tar.zst) zstd -dc "$src" | tar -x -C x ;;
        *.tgz)     tar -xzf "$src" -C x ;;
        *.zip)     unzip -q "$src" -d x ;;
      esac
      tar -C x -cf - . | pigz > "$out"
    '' ];
    src = fetch a;
    assetName = a.name;
  };
  ollamaComponents = pkgs.linkFarm "ollama-components"
    (map (a: { name = "ollama-${ollamaKeyOf a.name}.tar.gz"; path = repackOllama a; }) vendorLock.ollama.assets);
in
{ inherit vendor ollamaComponents; }
