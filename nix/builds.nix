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
  # git+file (not a bare path) so the flake source is the GIT tree — tracked files
  # only. A bare `toString ../.` is a `path:` flakeref that copies the WHOLE repo,
  # including the multi-GB gitignored dist/, into the store on every nix-build
  # (filled the disk + broke when a dist/ file changed mid-copy).
  flake = builtins.getFlake "git+file://${toString ../.}";
  pkgs = flake.inputs.nixpkgs.legacyPackages.${builtins.currentSystem};
  lib = pkgs.lib;
  stores = import ./stores.nix;
  ollamaComponents = flake.packages.${builtins.currentSystem}.ollamaComponents;

  # squashfs of a source dir — same mksquashfs flags as scripts/lib.sh pack_squashfs,
  # so the loader mounts it identically. nix owns the build + content-addressed cache.
  mkSqfs = name: src: pkgs.runCommand "${name}.squashfs"
    { nativeBuildInputs = [ pkgs.squashfsTools ]; }
    "mksquashfs ${src} $out -comp zstd -processors $NIX_BUILD_CORES -all-root -no-xattrs -noappend -quiet";

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
  # memSize must exceed the bare HFS+ raw (it lives in the VM's RAM tmpfs) — bump it
  # for large components (the runtime). Host has 30G; 6G is plenty for a ~2.7G raw.
  mkDmg = { name, src, vol ? "PlanAI", memSize ? 2048 }:
    vmTools.runInLinuxVM (pkgs.runCommand "${name}.dmg"
      { nativeBuildInputs = [ pkgs.hfsprogs pkgs.util-linux ]; inherit memSize; }
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

  # --- mac launcher .app, wrapped + ad-hoc signed in nix ---------------------
  # The standalone launcher binary is already nix (launcher-mac-arm64). Wrap it in
  # a plan.ai.app + ad-hoc code-sign with rcodesign (fully OFFLINE — no cert, no
  # network), so the launcher dmg needs no host sudo loop-mount (mkDmg packs it in a
  # VM; its mount + cp -a preserves the exec bit + signature). Real cert signing
  # reads a secret p12 → impure, so MAC_P12 builds stay imperative in bundle.sh;
  # this is the ad-hoc default. The .app's MacOS exe IS the launcher; at runtime it
  # mounts app-mac-arm64.dmg from the pool + runs the real Electron app from it.
  launcherMac = flake.packages.${builtins.currentSystem}.launcher-mac-arm64;
  appVersion = (builtins.fromJSON (builtins.readFile (flake.outPath + "/app/package.json"))).version;
  launcherInfoPlist = pkgs.writeText "Info.plist" ''
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
      <key>CFBundleName</key><string>plan.ai</string>
      <key>CFBundleDisplayName</key><string>plan.ai</string>
      <key>CFBundleIdentifier</key><string>ai.plan.usb.launcher</string>
      <key>CFBundleVersion</key><string>${appVersion}</string>
      <key>CFBundleShortVersionString</key><string>${appVersion}</string>
      <key>CFBundlePackageType</key><string>APPL</string>
      <key>CFBundleExecutable</key><string>plan-ai</string>
      <key>LSMinimumSystemVersion</key><string>11.0</string>
      <key>NSHighResolutionCapable</key><true/>
    </dict></plist>
  '';
  launcherMacApp = pkgs.runCommand "plan-ai-launcher-app" { nativeBuildInputs = [ pkgs.rcodesign ]; } ''
    app=$out/plan.ai.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp ${launcherMac}/plan-ai "$app/Contents/MacOS/plan-ai"
    chmod 0755 "$app/Contents/MacOS/plan-ai"
    cp ${launcherInfoPlist} "$app/Contents/Info.plist"
    printf 'APPL????' > "$app/Contents/PkgInfo"
    rcodesign sign "$app"
  '';

  # --- NixOS FHS helper closure, exported PURELY in the sandbox --------------
  # The launcher imports this closure on NixOS first-run (`nix-store --import`,
  # daemon-mediated → works for any trusted user) so the FHS wrapper + its libs land
  # in the user's store. We produce the `nix-store --import` stream WITHOUT a host
  # `nix-store --export`: closureInfo gives the closure's store-paths + a
  # `nix-store --load-db` registration via exportReferencesGraph — evaluated by the
  # outer nix and fed in as data (no daemon needed; the paths are mounted into the
  # sandbox because closureInfo's output text references them). We seed a throwaway
  # local nix DB from that registration (NIX_STATE_DIR in $TMPDIR — the default
  # /nix/var/nix is read-only in the sandbox) and `nix-store --export` against it.
  # Pattern adapted from nixpkgs nixos/lib/make-squashfs.nix (same closureInfo data).
  nixosFhs = flake.packages.${builtins.currentSystem}.nixosFhs;
  nixosFhsClosureInfo = pkgs.closureInfo { rootPaths = [ nixosFhs ]; };
in
{
  # ollama: linux squashfs, darwin dmg, windows dir (source = the nix repack)
  "ollama-linux-amd64-squashfs" = mkSqfs "ollama-linux-amd64" (mkOllamaDir "linux-amd64");
  "ollama-linux-arm64-squashfs" = mkSqfs "ollama-linux-arm64" (mkOllamaDir "linux-arm64");
  "ollama-linux-amd64-rocm-squashfs" = mkSqfs "ollama-linux-amd64-rocm" (mkOllamaDir "linux-amd64-rocm");
  "ollama-darwin-dmg" = mkDmg { name = "ollama-darwin"; src = mkOllamaDir "darwin"; };
  "ollama-windows-amd64-dir" = mkOllamaDir "windows-amd64";
  # ow-assets: linux squashfs + mac dmg (source = the store-imported hf+nltk assets)
  "ow-assets-squashfs" = mkSqfs "ow-assets" (stores.ow-assets or (throw "ow-assets not imported"));
  "ow-assets-dmg" = mkDmg { name = "ow-assets"; src = stores.ow-assets or (throw "ow-assets not imported"); };
  # runtimes: store-import the built dist/runtime/<t> (python tree + runtime.json that
  # make-runtime already writes), then pack — linux squashfs, mac dmg, win dir.
  "runtime-linux-x64-squashfs" = mkSqfs "runtime-linux-x64" (stores.runtime-linux-x64 or (throw "runtime-linux-x64 not imported"));
  "runtime-mac-arm64-dmg" = mkDmg { name = "runtime-mac-arm64"; src = stores.runtime-mac-arm64 or (throw "runtime-mac-arm64 not imported"); memSize = 6144; };
  # win dir component needs no packing (used in place) — a derivation that just
  # materialises the imported tree, so nix-build -A has something to build.
  "runtime-win-x64-dir" = pkgs.runCommand "runtime-win-x64" { }
    "mkdir -p $out && cp -a ${stores.runtime-win-x64 or (throw "runtime-win-x64 not imported")}/. $out/";

  # the Electron app itself, packed as a component (app-<os>) — same per-OS formats
  # as the runtime. electron-builder/@electron/packager produce the unpacked tree
  # impurely (downloads + helper patching), so bundle.sh store-imports that tree
  # (already signed, for mac) and these pack it OFFLINE: linux squashfs, mac dmg
  # (the VM mount + cp -a preserves the .app's exec bit + signature), win dir.
  "app-linux-x64-squashfs" = mkSqfs "app-linux-x64" (stores.app-linux-x64 or (throw "app-linux-x64 not imported"));
  "app-mac-arm64-dmg" = mkDmg { name = "app-mac-arm64"; src = stores.app-mac-arm64 or (throw "app-mac-arm64 not imported"); };
  "app-win-x64-dir" = pkgs.runCommand "app-win-x64" { }
    "mkdir -p $out && cp -a ${stores.app-win-x64 or (throw "app-win-x64 not imported")}/. $out/";

  # the mac LAUNCHER dmg (plan-ai.dmg): the ad-hoc-signed plan.ai.app packed by mkDmg
  # (volume "plan.ai" — the user mounts it, then double-clicks plan.ai.app). No sudo.
  "launcher-mac-arm64-dmg" = mkDmg { name = "plan-ai-launcher-mac"; src = launcherMacApp; vol = "plan.ai"; };

  # the NixOS FHS helper closure as a `nix-store --import` stream (see notes above).
  "nixos-fhs-closure" = pkgs.runCommand "nixos-fhs.closure" { nativeBuildInputs = [ pkgs.nix ]; } ''
    export NIX_STATE_DIR=$TMPDIR/state NIX_LOG_DIR=$TMPDIR/log
    mkdir -p "$NIX_STATE_DIR" "$NIX_LOG_DIR"
    nix-store --load-db < ${nixosFhsClosureInfo}/registration
    nix-store --export $(cat ${nixosFhsClosureInfo}/store-paths) > $out
  '';

  # --- the FAT32 USB image (the capstone) ------------------------------------
  # make-usb-image.sh assembles a drive-root dir (launchers + components/<os>/ +
  # models + update.json + platforms.json + README) IMPERATIVELY (the manifest gen
  # is xtask; models are the seeded cache), store-imports it, and this packs it into
  # a ready-to-burn FAT32 image OFFLINE — mkfs.vfat + mcopy via mtools, all
  # userspace (no VM, no sudo; mechanism proven). Size = drive-root + 15% + 128M
  # slack. Each per-file FAT32 4 GiB guard is enforced in the staging script.
  "usb-image" = pkgs.runCommand "plan-ai-usb.img"
    { nativeBuildInputs = [ pkgs.mtools pkgs.dosfstools pkgs.coreutils pkgs.findutils ]; }
    ''
      src=${stores.drive-root or (throw "drive-root not imported")}
      bytes=$(du -sb "$src" | cut -f1)
      mb=$(( bytes / 1048576 * 115 / 100 + 128 ))
      echo "FAT32 image: ''${mb}MB from drive-root $(du -sh "$src" | cut -f1)"
      truncate -s "''${mb}M" "$out"
      mkfs.vfat -F 32 -n PLANAI "$out" >/dev/null
      # copy the drive-root contents (not the dir itself) to the image root. `*`
      # skips dotfiles — the drive-root has none. mcopy -s recurses incl. empty dirs.
      mcopy -i "$out" -s -Q -b "$src"/* ::/
      mmd -i "$out" ::/data 2>/dev/null || true
      echo "contents:"; mdir -i "$out" :: 2>/dev/null | sed 's/^/    /' || true
    '';

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
}
# expose each import directly too (handy for `nix-build --impure nix/builds.nix -A <name>`)
// stores
