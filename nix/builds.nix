# Pure-nix build layer for store-imported (impure-fetched) inputs.
#
# The migration shape: a thin imperative layer fetches the genuinely-networked bits,
# `third_party/loader/scripts/store-import.sh` adds each to the store + pins a GC root and records its
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

  # The generic component packers (mkSqfs / mkClosureSqfs / mkExtractDir / mkDmg)
  # now live in the shared loader nix lib (third_party/loader/nix/loader), so any
  # consuming product reuses them. We keep the historical local names so the
  # derivation table below is unchanged. mkDmg is curried with our libdmg pin.
  loader = flake.inputs.loader.loaderLib { inherit pkgs; nixpkgs = flake.inputs.nixpkgs; };
  libdmg = flake.packages.${builtins.currentSystem}.libdmg-hfsplus;
  mkSqfs = loader.mkSqfs;
  mkClosureSqfs = loader.mkClosureSqfs;
  mkDmg = loader.mkDmg { inherit libdmg; };

  # extract an ollama/llama.cpp flavour to a plain dir (repack is already a store path).
  mkOllamaDir = key: loader.mkExtractDir "ollama-${key}" "${ollamaComponents}/ollama-${key}.tar.gz";
  llamacppComponents = flake.packages.${builtins.currentSystem}.llamacppComponents;
  mkLlamacppDir = key: loader.mkExtractDir "llamacpp-${key}" "${llamacppComponents}/llamacpp-${key}.tar.gz";
  # linux flavours: the prebuilt binaries' libllama-server-impl.so / libllama-common.so
  # link libssl.so.3 + libcrypto.so.3 (HTTPS support) but the tarballs don't ship them.
  # Drop a copy in build/bin/ssl-fallback/ — NOT on the default lib path. The usbd
  # llamacpp service adds that dir to LD_LIBRARY_PATH ONLY when the target has no
  # system libssl.so.3, so a working (often older-glibc) system libssl is never
  # overridden by ours (nixpkgs openssl is glibc-2.38). Per-arch: an aarch64 component
  # needs aarch64 openssl, substituted from the binary cache — we only copy the .so
  # files (no execution), so building it on x86_64 is fine.
  opensslLibDir = sys: "${flake.inputs.nixpkgs.legacyPackages.${sys}.openssl.out}/lib";
  mkLlamacppLinuxDir = key: sys: pkgs.runCommand "llamacpp-${key}" { nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ]; } ''
    mkdir -p $out && tar -xf ${llamacppComponents}/llamacpp-${key}.tar.gz -C $out
    bindir="$(dirname "$(find $out -name llama-server -type f | head -1)")"
    [ -n "$bindir" ] || { echo "llamacpp-${key}: no llama-server in tarball"; exit 1; }
    mkdir -p "$bindir/ssl-fallback"
    cp -L ${opensslLibDir sys}/libssl.so.3 ${opensslLibDir sys}/libcrypto.so.3 "$bindir/ssl-fallback/"
    chmod -R u+w "$bindir/ssl-fallback"
  '';
  # windows flavours: upstream's win zips don't ship the MSVC C++ runtime, and
  # llama-server.exe links VCRUNTIME140/MSVCP140 — on a machine without the VC++
  # redist it dies before main(). Drop the shared-pinned redist DLLs
  # (nix/msvc-runtime.nix, same pin as the python runtime + llmfit.exe) at the
  # component root beside llama-server.exe. -n: never clobber a DLL upstream
  # starts shipping itself.
  msvcDlls = loader.msvcDlls;
  mkLlamacppWinDir = key: pkgs.runCommand "llamacpp-${key}" { nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ]; } ''
    mkdir -p $out && tar -xf ${llamacppComponents}/llamacpp-${key}.tar.gz -C $out
    cp -n ${msvcDlls}/*.dll $out/
  '';

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
  # in the user's store. closureInfo gives the closure's store-paths; mkClosureSqfs
  # (above) packs them into a squashfs the launcher mounts + bind/overlays as
  # /nix/store on NixOS — no `nix-store --import`, so no trusted-user requirement.
  nixosFhs = flake.packages.${builtins.currentSystem}.nixosFhs;
  nixosFhsArm64 = flake.packages.${builtins.currentSystem}.nixosFhs-arm64;
  nixosFhsClosureInfo = pkgs.closureInfo { rootPaths = [ nixosFhs ]; };
  nixosFhsArm64ClosureInfo = pkgs.closureInfo { rootPaths = [ nixosFhsArm64 ]; };
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
  # usbd: the plan.ai USB daemon binary at the component root (usbd[.exe]), so
  # the launcher resolves <usbd>/usbd[.exe] under PLANAI_RESOURCES. Per-TARGET
  # in the shared pool (like runtime-*) so two linux arches don't collide; bundle.sh
  # renames the matching one to the fixed `usbd` name in each OS group dir. Per-OS
  # formats: linux squashfs, mac dmg, win dir. Win/mac/linux-arm64 are cross-built
  # (cargo-zigbuild — see flake `usbdFor`); linux-x64 is the native `usbd`.
  "usbd-linux-x64-squashfs" = mkSqfs "usbd-linux-x64" (flake.packages.${builtins.currentSystem}.usbdComponent);
  "usbd-linux-arm64-squashfs" = mkSqfs "usbd-linux-arm64" (flake.packages.${builtins.currentSystem}.usbdComponent-linux-arm64);
  "usbd-mac-arm64-dmg" = mkDmg { name = "usbd-mac-arm64"; src = flake.packages.${builtins.currentSystem}.usbdComponent-mac-arm64; };
  "usbd-win-x64-dir" = pkgs.runCommand "usbd-win-x64" { }
    "mkdir -p $out && cp -a ${flake.packages.${builtins.currentSystem}.usbdComponent-win-x64}/. $out/";
  # llama.cpp: the optional llama-server component (feature "llamacpp",
  # default-off), one component per flavour like ollama — the launcher picks the
  # flavour by GPU detection (vulkan vs cpu; mac is Metal-always).
  "llamacpp-linux-amd64-squashfs" = mkSqfs "llamacpp-linux-amd64" (mkLlamacppLinuxDir "linux-amd64" "x86_64-linux");
  "llamacpp-linux-amd64-vulkan-squashfs" = mkSqfs "llamacpp-linux-amd64-vulkan" (mkLlamacppLinuxDir "linux-amd64-vulkan" "x86_64-linux");
  "llamacpp-linux-arm64-squashfs" = mkSqfs "llamacpp-linux-arm64" (mkLlamacppLinuxDir "linux-arm64" "aarch64-linux");
  "llamacpp-linux-arm64-vulkan-squashfs" = mkSqfs "llamacpp-linux-arm64-vulkan" (mkLlamacppLinuxDir "linux-arm64-vulkan" "aarch64-linux");
  "llamacpp-darwin-dmg" = mkDmg { name = "llamacpp-darwin"; src = mkLlamacppDir "darwin"; };
  "llamacpp-windows-amd64-dir" = mkLlamacppWinDir "windows-amd64";
  "llamacpp-windows-amd64-vulkan-dir" = mkLlamacppWinDir "windows-amd64-vulkan";

  # hermes-webui: the lightweight hermes web UI (same "hermes" feature —
  # classify_feature matches the basename prefix). Pure python/static sources,
  # ONE shared component for all platforms (like ow-assets): linux squashfs,
  # mac dmg, win dir (zipped by the win component packer). Runs on the hermes
  # component's portable python (see usbd's hermes-webui service).
  "hermes-webui-squashfs" = mkSqfs "hermes-webui" (flake.packages.${builtins.currentSystem}.hermes-webui);
  "hermes-webui-dmg" = mkDmg { name = "hermes-webui"; src = flake.packages.${builtins.currentSystem}.hermes-webui; };
  "hermes-webui-dir" = pkgs.runCommand "hermes-webui-dir" { }
    "mkdir -p $out && cp -a ${flake.packages.${builtins.currentSystem}.hermes-webui}/. $out/";

  # hermes: the optional hermes-agent component (feature "hermes", default-off in
  # platforms.json — not on the image / not downloaded until enabled). Source is
  # fully pure nix (flake `hermes-<target>`, wheels-FOD into pbs — nix/hermes.nix);
  # packed per-OS like usbd: linux squashfs, mac dmg, win dir (zipped by
  # nix-component.sh when the out path ends .zip).
  "hermes-linux-x64-squashfs" = mkSqfs "hermes-linux-x64" (flake.packages.${builtins.currentSystem}.hermes-linux-x64);
  "hermes-linux-arm64-squashfs" = mkSqfs "hermes-linux-arm64" (flake.packages.${builtins.currentSystem}.hermes-linux-arm64);
  "hermes-mac-arm64-dmg" = mkDmg { name = "hermes-mac-arm64"; src = flake.packages.${builtins.currentSystem}.hermes-mac-arm64; memSize = 4096; };
  "hermes-win-x64-dir" = pkgs.runCommand "hermes-win-x64" { }
    "mkdir -p $out && cp -a ${flake.packages.${builtins.currentSystem}.hermes-win-x64}/. $out/";

  # runtimes: store-import the built dist/runtime/<t> (python tree + runtime.json that
  # make-runtime already writes), then pack — linux squashfs, mac dmg, win dir.
  "runtime-linux-x64-squashfs" = mkSqfs "runtime-linux-x64" (stores.runtime-linux-x64 or (throw "runtime-linux-x64 not imported"));
  "runtime-linux-arm64-squashfs" = mkSqfs "runtime-linux-arm64" (stores.runtime-linux-arm64 or (throw "runtime-linux-arm64 not imported"));
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
  "app-linux-arm64-squashfs" = mkSqfs "app-linux-arm64" (stores.app-linux-arm64 or (throw "app-linux-arm64 not imported"));
  "app-mac-arm64-dmg" = mkDmg { name = "app-mac-arm64"; src = stores.app-mac-arm64 or (throw "app-mac-arm64 not imported"); };
  "app-win-x64-dir" = pkgs.runCommand "app-win-x64" { }
    "mkdir -p $out && cp -a ${stores.app-win-x64 or (throw "app-win-x64 not imported")}/. $out/";

  # the mac LAUNCHER dmg (plan-ai.dmg): the ad-hoc-signed plan.ai.app packed by mkDmg
  # (volume "plan.ai" — the user mounts it, then double-clicks plan.ai.app). No sudo.
  "launcher-mac-arm64-dmg" = mkDmg { name = "plan-ai-launcher-mac"; src = launcherMacApp; vol = "plan.ai"; };

  # the NixOS FHS helper closure as a SQUASHFS (mounted + bind/overlaid as /nix/store
  # on NixOS, no import), one per linux arch (bundle.sh picks the one matching target).
  "nixos-fhs-squashfs-x64" = mkClosureSqfs nixosFhsClosureInfo;
  "nixos-fhs-squashfs-arm64" = mkClosureSqfs nixosFhsArm64ClosureInfo;

  # --- the FAT32 USB image ----------------------------------------------------
  # The image is packed OUTSIDE the nix store by third_party/loader/scripts/make-usb-image.sh: it
  # mkfs.vfat + mcopy's the on-disk drive-root (launchers + components/<os>/ +
  # models + update.json + platforms.json + README) with the pinned userspace
  # tooling from `.#usb-image-tools`. We intentionally do NOT have a `usb-image`
  # derivation here: store-importing the drive-root would duplicate the whole
  # multi-GB tree (incl. seeded models) into /nix/store. Packing a file from a
  # folder we already have on disk needs no sandbox, so it stays out-of-store.

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
