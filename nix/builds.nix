# Pure-nix build layer for store-imported (impure-fetched) inputs.
#
# The migration shape: a thin imperative layer fetches the genuinely-networked bits,
# `scripts/store-import.sh` adds each to the store + pins a GC root and records its
# path under dist/.stores/<name>; ./stores.nix reads those records into storePaths.
# The derivations here then consume those store paths and build OFFLINE in the
# sandbox, so nix owns the graph + caching + invalidation for everything past the fetch.
#
# Evaluate with --impure (builtins.storePath, builtins.currentSystem, getFlake on the
# dirty tree, and reading dist/.stores all require it):
#   nix-build --impure nix/builds.nix -A <attr>
# We reuse the flake's pinned nixpkgs so these match the rest of the build.
#
# (A from-source open_webui wheel/frontend built on this same foundation lives in
# archive/from-source-openwebui/ — not shipped; the runtime uses the PyPI wheel FOD.)
let
  flake = builtins.getFlake (toString ../.);
  pkgs = flake.inputs.nixpkgs.legacyPackages.${builtins.currentSystem};
  lib = pkgs.lib;
  stores = import ./stores.nix;
  ollamaComponents = flake.packages.${builtins.currentSystem}.ollamaComponents;

  # An ollama flavour as a squashfs, built in nix from the ollamaComponents repack
  # (already a nix store path — no store-import needed). Same mksquashfs flags as
  # pack-component, so the loader mounts it identically.
  mkOllamaSqfs = key: pkgs.runCommand "ollama-${key}.squashfs"
    { nativeBuildInputs = [ pkgs.squashfsTools pkgs.gnutar pkgs.gzip ]; }
    ''
      mkdir ex && tar -xf ${ollamaComponents}/ollama-${key}.tar.gz -C ex
      mksquashfs ex $out -comp zstd -processors $NIX_BUILD_CORES -all-root -no-xattrs -noappend -quiet
    '';

  # --- mac dmg components, built in a Linux VM ------------------------------
  # An HFS+ dmg needs a privileged loop-mount, impossible in a plain sandbox. So
  # build it inside a QEMU VM (fast with KVM) where we're root and the kernel has
  # loop + hfsplus. The real mount + `cp -a` preserves symlinks + exec bits +
  # signatures (a .app needs them); hard links are copied as independent files
  # (the Linux HFS+ driver can't create them — same as lib.sh pack_dmg). libdmg
  # then compresses the bare HFS+ to a UDIF dmg. This makes dmgs pure, cached nix
  # derivations instead of host sudo calls.
  libdmg = flake.packages.${builtins.currentSystem}.libdmg-hfsplus;
  vmTools = pkgs.vmTools.override {
    rootModules = [ "virtio_pci" "virtio_mmio" "virtio_blk" "virtio_balloon"
      "virtio_rng" "ext4" "virtiofs" "crc32c" "loop" "hfsplus" ];
  };
  mkDmg = { name, src, vol ? "PlanAI" }:
    vmTools.runInLinuxVM (pkgs.runCommand "${name}.dmg"
      { nativeBuildInputs = [ pkgs.hfsprogs pkgs.util-linux ]; memSize = 2048; }
      ''
        raw=$TMPDIR/raw.hfs
        # size from du -l (each hard-link name counted, since cp breaks them) + a
        # per-file catalog pad + slack; over-provision is free (UDIF compresses it).
        kb=$(du -slk ${src} | cut -f1); nfiles=$(find ${src} | wc -l)
        truncate -s $(( kb * 1024 + nfiles * 4096 + 256*1024*1024 )) $raw
        mkfs.hfsplus -v "${vol}" $raw
        mkdir -p $TMPDIR/mnt
        mount -t hfsplus -o loop $raw $TMPDIR/mnt
        cp -a --no-preserve=links ${src}/. $TMPDIR/mnt/
        umount $TMPDIR/mnt
        ${libdmg}/bin/dmg dmg $raw $out
      '');
  # an ollama flavour extracted to a plain directory (the repack is already a store
  # path). The windows component ships as a dir (used in place on FAT32); darwin's
  # extracted tree feeds mkDmg.
  mkOllamaDir = key: pkgs.runCommand "ollama-${key}" { nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ]; }
    "mkdir -p $out && tar -xf ${ollamaComponents}/ollama-${key}.tar.gz -C $out";
in
{
  "ollama-linux-amd64-squashfs" = mkOllamaSqfs "linux-amd64";
  "ollama-linux-arm64-squashfs" = mkOllamaSqfs "linux-arm64";
  "ollama-linux-amd64-rocm-squashfs" = mkOllamaSqfs "linux-amd64-rocm";
  "ow-assets-dmg" = mkDmg { name = "ow-assets"; src = stores.ow-assets or (throw "ow-assets not imported"); };
  "ollama-darwin-dmg" = mkDmg { name = "ollama-darwin"; src = mkOllamaDir "darwin"; };
  "ollama-windows-amd64-dir" = mkOllamaDir "windows-amd64";

  # Smoke proof: a pure derivation consuming every imported store path, showing the
  # fetch -> store-add -> gcroot -> record -> storePath chain feeds offline nix builds.
  # Each ${p} both interpolates the path AND registers it as a real build input (a bare
  # string would NOT be mounted into the sandbox).
  stores-proof = pkgs.runCommand "stores-proof" { } ''
    mkdir -p $out
    echo "imported store paths consumed by nix:" > $out/report.txt
    ${lib.concatStringsSep "\n" (lib.mapAttrsToList (n: p: ''
      test -e ${p} || { echo "MISSING import ${n}"; exit 1; }
      echo "  ${n} -> ${p} ($(du -sh ${p} | cut -f1))" >> $out/report.txt
    '') stores)}
    cat $out/report.txt
  '';

  # ow-assets component as a squashfs, built in nix from the store-imported assets
  # (the networked hf+nltk prefetch stays imperative). Same mksquashfs flags as
  # scripts/lib.sh pack_squashfs, so the loader mounts it identically — nix just
  # owns the build (cached/invalidated by the import's content hash). The mac dmg +
  # win dir formats stay in pack-component.sh (dmg needs a privileged loop-mount).
  ow-assets-squashfs = pkgs.runCommand "ow-assets.squashfs"
    { nativeBuildInputs = [ pkgs.squashfsTools ]; }
    ''
      mksquashfs ${stores.ow-assets or (throw "ow-assets not imported — run store-import.sh ow-assets vendor/ow-assets")} \
        $out -comp zstd -processors $NIX_BUILD_CORES -all-root -no-xattrs -noappend -quiet
    '';
}
# expose each import directly too (handy for `nix-build --impure nix/builds.nix -A <name>`)
// stores
