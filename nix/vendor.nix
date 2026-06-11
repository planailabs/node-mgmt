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
    ++ lib.optional (vendorLock ? hermes)
      { name = "hermes/${vendorLock.hermes.tag}/source.tar.gz"; path = fetch vendorLock.hermes; }
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
      # gzip is required: `tar -z` (the darwin .tgz path) execs the gzip program;
      # pigz alone is not enough and darwin would repack EMPTY (linux=.tar.zst,
      # win=.zip don't hit it).
      export PATH="${lib.makeBinPath (with pkgs; [ coreutils gnutar zstd gzip pigz unzip ])}"
      set -e
      mkdir x
      case "$assetName" in
        *.tar.zst) zstd -dc "$src" | tar -x -C x ;;
        *.tgz|*.tar.gz) tar -xzf "$src" -C x ;;
        *.zip)     unzip -q "$src" -d x ;;
      esac
      tar -C x -cf - . | pigz > "$out"
    '' ];
    src = fetch a;
    assetName = a.name;
  };
  ollamaComponents = pkgs.linkFarm "ollama-components"
    (map (a: { name = "ollama-${ollamaKeyOf a.name}.tar.gz"; path = repackOllama a; }) vendorLock.ollama.assets);

  # llama.cpp prebuilt release assets, normalised to uniform tar.gz components
  # keyed like ollama's flavours (llamacpp-<key>.tar.gz). Upstream's asset name
  # `llama-<tag>-bin-<plat>[.-…]` maps to our key:
  #   ubuntu-x64 → linux-amd64 · ubuntu-vulkan-x64 → linux-amd64-vulkan
  #   ubuntu-arm64 → linux-arm64 · ubuntu-vulkan-arm64 → linux-arm64-vulkan
  #   macos-arm64 → darwin · win-cpu-x64 → windows-amd64 ·
  #   win-vulkan-x64 → windows-amd64-vulkan
  llamacppKeyOf = name: lib.pipe name [
    (lib.removePrefix "llama-${vendorLock.llamacpp.tag}-bin-")
    (lib.removeSuffix ".tar.gz") (lib.removeSuffix ".zip")
    (n: {
      "ubuntu-x64" = "linux-amd64";
      "ubuntu-vulkan-x64" = "linux-amd64-vulkan";
      "ubuntu-arm64" = "linux-arm64";
      "ubuntu-vulkan-arm64" = "linux-arm64-vulkan";
      "macos-arm64" = "darwin";
      "win-cpu-x64" = "windows-amd64";
      "win-vulkan-x64" = "windows-amd64-vulkan";
    }.${n} or n)
  ];
  # The same vanilla no-fixup repack as ollama, plus a flatten: the linux/mac
  # tarballs nest everything under a single `llama-<tag>/` dir (win zips are
  # flat) — flatten so `llama-server` + its libs sit at the component root.
  repackLlamacpp = a: derivation {
    inherit system;
    name = "llamacpp-${llamacppKeyOf a.name}.tar.gz";
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      export PATH="${lib.makeBinPath (with pkgs; [ coreutils gnutar gzip pigz unzip ])}"
      set -e
      mkdir x
      case "$assetName" in
        *.tgz|*.tar.gz) tar -xzf "$src" -C x ;;
        *.zip)          unzip -q "$src" -d x ;;
      esac
      if [ "$(ls x | wc -l)" = 1 ] && [ -d "x/$(ls x)" ]; then
        inner="x/$(ls x)"
        mv "$inner" flat && rmdir x && mv flat x
      fi
      tar -C x -cf - . | pigz > "$out"
    '' ];
    src = fetch a;
    assetName = a.name;
  };
  llamacppComponents = pkgs.linkFarm "llamacpp-components"
    (map (a: { name = "llamacpp-${llamacppKeyOf a.name}.tar.gz"; path = repackLlamacpp a; })
      (vendorLock.llamacpp.assets or [ ]));
in
{ inherit vendor ollamaComponents llamacppComponents; }
