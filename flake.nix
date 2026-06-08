{
  description = "plan-ai-usb-minimal — portable offline Ollama + Open-WebUI + Electron stack";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # rust toolchain with cross-target std (for the native launcher, cross-built
    # win/mac from NixOS via cargo-zigbuild). Pattern from plan-ai/mac-mgmt.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # mac-mgmt provides mac-mgmt-services (the cross-platform process supervisor the
    # launcher's control plane drives). flake=false: we consume the crate source,
    # copied into the launcher's vendor/ at build time. Pinned by rev for
    # reproducibility; bump after landing changes in mac-mgmt.
    mac-mgmt = {
      url = "git+ssh://git@git.plan.ai/plan-ai/mac-mgmt?ref=trunk&rev=ea420ee4443e91c667422bd4b368e6b763ef8e46";
      flake = false;
    };
    # Include git submodules in the flake source — the SPA build needs
    # third_party/plan-ai-design (the shared Dioxus component library), which is a
    # submodule and would otherwise be excluded from the flake source tree.
    self.submodules = true;
  };

  # Thin entrypoint — the real definitions live in nix/:
  #   nix/devshell.nix  the dev shell (toolchain + NixOS env)
  #   nix/vendor.nix    layer 1 download FODs + layer 2 no-fixup ollama repack
  #   nix/runtime.nix   layer 3 portable open-webui runtime (wheels-FOD + vanilla install)
  outputs = { self, nixpkgs, flake-utils, rust-overlay, mac-mgmt }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
        lib = pkgs.lib;

        # rust toolchain with the cross-target std libs the launcher needs
        rustToolchain = pkgs.rust-bin.stable.latest.minimal.override {
          targets = [ "x86_64-pc-windows-gnu" "aarch64-apple-darwin"
                      "x86_64-unknown-linux-gnu" "x86_64-unknown-linux-musl" ];
        };
        # Full stable toolchain + wasm32 for the Dioxus SPA (dx/cargo need a full
        # rustc for wasm32-unknown-unknown).
        wasmToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };
        wasmRustPlatform = pkgs.makeRustPlatform { cargo = wasmToolchain; rustc = wasmToolchain; };

        # macOS SDK source (Cocoa headers + framework stubs) for cross-compiling
        # crates with native Apple deps — notify-rust's mac-notification-sys
        # #imports <Cocoa/Cocoa.h>. Fed to the mac launcher build via SDKROOT so
        # zig cc finds the frameworks. Source-only (a fetch), so it builds on
        # linux despite being from the darwin package set.
        macosx-sdk = let
          darwinPkgs = import nixpkgs { system = "aarch64-darwin"; };
        in darwinPkgs.apple-sdk_26.src;

        # Dioxus `feat/embed` fork git-dep hashes (same rev mac-mgmt pins). dx is
        # nixpkgs' stock 0.7.9 — it prints a non-fatal "incompatible" notice for
        # 0.8-alpha but builds fine once the wasm-bindgen-cli version matches the
        # project's (pinned to 0.2.121 in spa-src/Cargo.toml).
        dioxusHash = "sha256-asz/Sm7BHGBNYvPXZS/rx+tlZrTbrmNNCoHal16LKzk=";
        dioxusI18nHash = "sha256-Y05EJtoJMw07aonrkIXp1gcDZKrhm30Aok7mClxAL78=";
        spaGitHashes = import ./launcher/spa-src/cargo-git-hashes.nix {
          inherit dioxusHash dioxusI18nHash;
        };
        spaTools = [
          wasmToolchain pkgs.dioxus-cli pkgs.wasm-bindgen-cli_0_2_121
          pkgs.binaryen pkgs.lld pkgs.tailwindcss_3 pkgs.nodejs_22
        ];

        # The plan.ai dashboard: a Dioxus 0.8 web/wasm SPA reusing plan-ai-design,
        # built to static assets (index.html + wasm + js + tailwind) that the
        # launcher rust-embeds and serves over localhost. `dx build` vendors the
        # fork via importCargoLock (offline); tailwind compiles the design CSS.
        spa = wasmRustPlatform.buildRustPackage {
          pname = "plan-ai-spa";
          version = "0.1.0";
          # whole flake source (git-tracked only): needs spa-src + the
          # third_party/plan-ai-design path dep.
          src = ./.;
          # the crate (and its Cargo.lock) live in this subdir.
          cargoRoot = "launcher/spa-src";
          buildAndTestSubdir = "launcher/spa-src";
          cargoLock = {
            lockFile = ./launcher/spa-src/Cargo.lock;
            outputHashes = spaGitHashes;
          };
          nativeBuildInputs = spaTools;
          buildPhase = ''
            runHook preBuild
            export HOME="$TMPDIR" CARGO_NET_OFFLINE=true
            cd launcher/spa-src
            tailwindcss -i ../../third_party/plan-ai-design/assets/input.css \
              -o assets/tailwind.css --config tailwind.config.js --minify
            dx build --platform web --release
            cd ../..
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp -r launcher/spa-src/target/dx/plan-ai-spa/release/web/public/. "$out/"
            runHook postInstall
          '';
          doCheck = false;
        };
        # The build orchestrator (host-native), built offline so the Makefile can
        # call it in CI (`nix run .#xtask`). Owns update-manifest generation +
        # tarball + (next) the ninja graph. Shares the plan-ai-manifest path dep
        # with the launcher updater, so src is the whole flake (xtask + crates/).
        xtask = pkgs.rustPlatform.buildRustPackage {
          pname = "xtask";
          version = "0.1.0";
          src = ./.;
          cargoRoot = "xtask";
          buildAndTestSubdir = "xtask";
          cargoLock.lockFile = ./xtask/Cargo.lock;
          doCheck = false;
          meta.mainProgram = "xtask";
        };
        # registry deps for the launcher's Cargo.lock (tokio, interprocess, …),
        # vendored offline. The mac-mgmt-services path dep is supplied separately
        # (copied from the mac-mgmt input into vendor/ in the build).
        launcherVendor = pkgs.rustPlatform.importCargoLock {
          lockFile = ./launcher/Cargo.lock;
        };
        # native launcher cross-compiled via cargo-zigbuild. It now carries deps
        # (the control plane), so the build vendors crates.io (launcherVendor) and
        # copies mac-mgmt-services from the mac-mgmt input into vendor/.
        launcherFor = { zigTarget, outDir }:
          let
            # The splash spinner for the MATCHING target, embedded into the launcher
            # via build.rs (PLANAI_SPINNER_BIN) — the launcher carries it instead of
            # shipping it in the pool. Linux launcher is musl but the spinner is gnu
            # (a GUI needs a dynamic loader); embedding raw bytes is target-agnostic.
            spinnerPkg =
              if lib.hasInfix "windows" zigTarget then
                spinnerFor { zigTarget = "x86_64-pc-windows-gnu"; outDir = "x86_64-pc-windows-gnu"; }
              else if lib.hasInfix "apple-darwin" zigTarget then
                spinnerFor { zigTarget = "aarch64-apple-darwin"; outDir = "aarch64-apple-darwin"; }
              else
                spinnerFor { zigTarget = "x86_64-unknown-linux-gnu"; outDir = "x86_64-unknown-linux-gnu"; };
            spinnerBin = "${spinnerPkg}/${if lib.hasInfix "windows" zigTarget then "plan-ai-spinner.exe" else "plan-ai-spinner"}";
          in
          pkgs.runCommand "plan-ai-launcher-${outDir}"
            ({
              nativeBuildInputs = [ rustToolchain pkgs.cargo-zigbuild pkgs.zig ];
              # build.rs embeds these into the linux launcher (mounts squashfs itself,
              # like the AppImage runtime); ignored for win/mac targets.
              PLANAI_SQUASHFUSE_LL = "${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll";
              PLANAI_UNSQUASHFS = "${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs";
              # the splash spinner, embedded into the launcher for every target.
              PLANAI_SPINNER_BIN = spinnerBin;
            } // lib.optionalAttrs (lib.hasInfix "apple-darwin" zigTarget) {
              # Cocoa headers/frameworks for notify-rust's mac-notification-sys.
              SDKROOT = macosx-sdk;
            })
            ''
              export HOME="$TMPDIR" CARGO_HOME="$TMPDIR/cargo" XDG_CACHE_HOME="$TMPDIR/cache"
              # path deps (../crates/*): copied as a sibling of src/ so Cargo resolves them.
              cp -r ${./crates} crates && chmod -R u+w crates
              cp -r ${./launcher}/. src && chmod -R u+w src && cd src
              # the SPA source isn't part of the launcher crate build; the built
              # web assets come from the `spa` derivation, embedded below.
              rm -rf spa-src
              rm -rf spa && cp -r ${spa} spa && chmod -R u+w spa
              # path dep: the standalone mac-mgmt-services crate from the input
              rm -rf vendor && mkdir -p vendor
              cp -r ${mac-mgmt}/mac-mgmt-services vendor/mac-mgmt-services
              chmod -R u+w vendor
              # crates.io deps from the vendored cargo lock
              mkdir -p .cargo
              printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s"\n' "${launcherVendor}" > .cargo/config.toml
              cargo zigbuild --release --offline --target ${zigTarget}
              mkdir -p "$out"
              for b in plan-ai plan-ai.exe; do
                if [ -f "target/${outDir}/release/$b" ]; then cp "target/${outDir}/release/$b" "$out/"; fi
              done
            '';

        # registry deps for the spinner crate's Cargo.lock (eframe + its tree).
        spinnerVendor = pkgs.rustPlatform.importCargoLock {
          lockFile = ./spinner/Cargo.lock;
        };
        # The native splash spinner (eframe/glow), cross-built like the launcher via
        # cargo-zigbuild. Unlike the launcher it CANNOT be static-musl (a GUI needs a
        # dynamic loader), so linux targets gnu. glow/winit dlopen the GL + windowing
        # libs at runtime, so the binary carries no nix-store paths and resolves the
        # TARGET machine's system libs (libGL/libX11/…). mac links AppKit/OpenGL etc.
        # from the Apple SDK via SDKROOT (same mechanism the launcher uses for Cocoa).
        spinnerFor = { zigTarget, outDir }:
          pkgs.runCommand "plan-ai-spinner-${outDir}"
            ({
              nativeBuildInputs = [ rustToolchain pkgs.cargo-zigbuild pkgs.zig ];
            } // lib.optionalAttrs (lib.hasInfix "apple-darwin" zigTarget) {
              SDKROOT = macosx-sdk;
            })
            ''
              export HOME="$TMPDIR" CARGO_HOME="$TMPDIR/cargo" XDG_CACHE_HOME="$TMPDIR/cache"
              cp -r ${./spinner}/. src && chmod -R u+w src && cd src
              mkdir -p .cargo
              printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s"\n' "${spinnerVendor}" > .cargo/config.toml
              cargo zigbuild --release --offline --target ${zigTarget}
              mkdir -p "$out"
              for b in plan-ai-spinner plan-ai-spinner.exe; do
                if [ -f "target/${outDir}/release/$b" ]; then cp "target/${outDir}/release/$b" "$out/"; fi
              done
            '';

        # llmfit — hardware-aware model selector. Bundled beside the launcher so it
        # can detect the GPU (`llmfit system --json`) and serve the model-browser API
        # (`llmfit serve`). Cross-building it from NixOS hits toolchain walls its heavy
        # deps need (win: synchronization.lib via parking_lot/windows-sys; mac:
        # libobjc/Apple SDK via objc2/sysinfo), so we ship UPSTREAM's sha256-pinned
        # prebuilt binaries (FODs). linux uses their static-musl build → runs on any
        # linux incl. NixOS, like our launcher. The url+sha256 pins live in
        # vendor.lock.json (.llmfit.assets), regenerated by scripts/gen-vendor-lock.sh
        # (bump usb.lock .llmfit.version, then `make update-locks`).
        llmfitBin = { url, sha256, ext, target }:
          let src = pkgs.fetchurl { inherit url sha256; };
          in pkgs.runCommand "llmfit-${target}"
            { nativeBuildInputs = [ pkgs.gnutar pkgs.gzip pkgs.unzip ]; }
            ''
              mkdir -p "$out" unpack && cd unpack
              ${if ext == "zip" then "unzip -q ${src}" else "tar xzf ${src}"}
              bin="$(find . -type f \( -name llmfit -o -name llmfit.exe \) | head -1)"
              [ -n "$bin" ] || { echo "no llmfit binary in archive" >&2; exit 1; }
              cp "$bin" "$out/$(basename "$bin")"; chmod +x "$out"/*
            '';

        vendorLock = builtins.fromJSON (builtins.readFile ./vendor.lock.json);
        vendorPkgs = import ./nix/vendor.nix { inherit pkgs lib system vendorLock; };

        # llmfit prebuilt asset for a release target, from vendor.lock.json
        llmfitAsset = t:
          lib.findFirst (a: a.target == t) (throw "no llmfit asset ${t} in vendor.lock.json")
            vendorLock.llmfit.assets;

        # python-build-standalone FOD for a target (the relocatable interpreter)
        pbsFor = target:
          let p = lib.findFirst (x: x.target == target) (throw "no pbs ${target}") vendorLock.pbs.files;
          in pkgs.fetchurl { inherit (p) url sha256; };

        # libdmg-hfsplus (fanquake fork — the one Bitcoin Core uses for
        # deterministic macOS dmgs). Its `dmg` tool wraps a raw HFS+ image into a
        # COMPRESSED UDIF (UDZO) .dmg on Linux — a proper, Finder-mountable dmg
        # without macOS/hdiutil (electron-builder's dmg is hdiutil-only). Pairs
        # with hfsprogs' mkfs.hfsplus. (fanquake's pure-Rust `libdmg` port was
        # tried but panics in libflate during compression — unusable.)
        # BUILD_SHARED_LIBS=OFF so the dmg/hfsplus tools statically link the
        # internal libs (no leftover /build rpath that nix rejects).
        libdmg-hfsplus = pkgs.stdenv.mkDerivation {
          pname = "libdmg-hfsplus";
          version = "unstable-2018-02-05";
          src = pkgs.fetchFromGitHub {
            owner = "fanquake";
            repo = "libdmg-hfsplus";
            rev = "7ac55ec64c96f7800d9818ce64c79670e7f02b67";
            hash = "sha256-5HHb08GEPzgLQC8y9YyhGoin1Oxy2UtOCx/4Xmb4ATQ=";
          };
          nativeBuildInputs = [ pkgs.cmake ];
          buildInputs = [ pkgs.zlib pkgs.bzip2 ];
          cmakeFlags = [
            "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"   # 2018 CMakeLists predates the cutoff
            "-DBUILD_SHARED_LIBS=OFF"
          ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/bin"
            cp dmg/dmg "$out/bin/dmg"
            cp hfs/hfsplus "$out/bin/hfsplus" 2>/dev/null || true
            runHook postInstall
          '';
          # cmake bakes a build-tree rpath; replace it with the real lib paths
          # BEFORE the fixup hooks audit for /build references (preFixup, not post).
          preFixup = ''
            for b in "$out/bin/dmg" "$out/bin/hfsplus"; do
              [ -f "$b" ] && patchelf --force-rpath --set-rpath "${lib.makeLibraryPath [ pkgs.zlib pkgs.bzip2 ]}" "$b"
            done
          '';
        };

        # portable runtime per target (wheels-FOD + vanilla install into pbs)
        runtimeFor = { target, triple, lockFile }:
          import ./nix/runtime.nix {
            inherit pkgs lib system triple;
            pyVersion = vendorLock.pbs.python;
            pbsArchive = pbsFor target;
            wheelsLock = builtins.fromJSON (builtins.readFile lockFile);
          };

        # uv cross-resolution triples per distributable target (mac-x64 dropped —
        # arm64-only macOS wheels). nixos-x64 reuses the linux-x64 runtime.
        runtimes = {
          runtime-linux-x64 = runtimeFor {
            target = "linux-x64"; triple = "x86_64-unknown-linux-gnu";
            lockFile = ./runtime/wheels-linux-x64.lock.json;
          };
          runtime-win-x64 = runtimeFor {
            target = "win-x64"; triple = "x86_64-pc-windows-msvc";
            lockFile = ./runtime/wheels-win-x64.lock.json;
          };
          runtime-mac-arm64 = runtimeFor {
            target = "mac-arm64"; triple = "aarch64-apple-darwin";
            lockFile = ./runtime/wheels-mac-arm64.lock.json;
          };
        };
        # Static squashfs tools (musl, no interpreter → run on ANY linux incl.
        # NixOS and stock Ubuntu) bundled into the linux/nixos artifacts so the
        # loader can mount components in place (squashfuse_ll) and extract as a
        # fallback (unsquashfs). Copied out of the store into the bundle.
        # AppImage type2 runtime (the small ELF prepended to the squashfs). We
        # assemble the AppImage by hand — runtime + mksquashfs(AppDir) — so the
        # AppRun can be our rust launcher (rust launches Electron). Pinned by hash.
        appimageRuntime = pkgs.fetchurl {
          url = "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64";
          hash = "sha256-okGdzkdWg5WuecAf+ppaNB3TOVgTUv8QTQc1J1Qxd+U=";
        };

        # NixOS FHS helper. NixOS's bare nix-ld stub can't run a generic glibc FHS
        # binary (the bundled Electron/ollama). buildFHSEnv gives a bubblewrap
        # sandbox that provides /lib64/ld-linux + the usual GUI/runtime libs under
        # an FHS layout. The static-musl launcher, on NixOS, imports this env's
        # closure (shipped as a NAR — its /nix/store paths don't exist on the
        # target) and re-execs ITSELF inside the wrapper, so the Electron it then
        # spawns inherits the FHS mount namespace and runs. runScript just execs
        # its args (so `planai-fhs <launcher> <args...>` runs the launcher in-FHS).
        nixosFhs = pkgs.buildFHSEnv {
          name = "planai-fhs";
          runScript = "${pkgs.writeShellScript "planai-fhs-run" ''exec "$@"''}";
          targetPkgs = p: with p; [
            glibc gcc-unwrapped.lib zlib
            glib gtk3 nss nspr atk at-spi2-atk at-spi2-core cairo pango gdk-pixbuf
            cups dbus expat libdrm libxkbcommon mesa libgbm alsa-lib
            freetype fontconfig libGL systemd
            libx11 libxcomposite libxcursor libxdamage
            libxext libxfixes libxi libxrender libxtst
            libxcb libxrandr libxscrnsaver
          ];
        };

        linuxMountTools = pkgs.runCommand "plan-ai-linux-mount-tools" { } ''
          mkdir -p "$out/bin"
          cp ${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll "$out/bin/squashfuse_ll"
          cp ${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs  "$out/bin/unsquashfs"
          chmod +x "$out/bin/"*
        '';
      in {
        packages = {
          inherit (vendorPkgs) vendor ollamaComponents;
          inherit linuxMountTools appimageRuntime nixosFhs spa macosx-sdk libdmg-hfsplus xtask;
          launcher-win-x64 = launcherFor { zigTarget = "x86_64-pc-windows-gnu"; outDir = "x86_64-pc-windows-gnu"; };
          launcher-mac-arm64 = launcherFor { zigTarget = "aarch64-apple-darwin"; outDir = "aarch64-apple-darwin"; };
          # linux: STATIC musl → zero dynamic-loader deps, so the launcher runs on
          # ANY linux incl. NixOS (whose bare nix-ld stub can't run a glibc FHS
          # binary). It autodetects NixOS at runtime and EXTRACTS components there
          # (the generic squashfs mount/patchelf path the node loader uses).
          launcher-linux-x64 = launcherFor { zigTarget = "x86_64-unknown-linux-musl"; outDir = "x86_64-unknown-linux-musl"; };
          # llmfit prebuilt binaries (bundled beside the launcher; GPU detect + serve).
          # Pins from vendor.lock.json (.llmfit.assets) — regen via gen-vendor-lock.sh.
          llmfit-linux-x64 = llmfitBin (llmfitAsset "x86_64-unknown-linux-musl");
          llmfit-win-x64   = llmfitBin (llmfitAsset "x86_64-pc-windows-msvc");
          llmfit-mac-arm64 = llmfitBin (llmfitAsset "aarch64-apple-darwin");
          # native splash spinner (shown while the launcher mounts the runtime).
          # linux=gnu (dynamic; a GUI can't be static-musl like the launcher).
          spinner-linux-x64 = spinnerFor { zigTarget = "x86_64-unknown-linux-gnu"; outDir = "x86_64-unknown-linux-gnu"; };
          spinner-win-x64   = spinnerFor { zigTarget = "x86_64-pc-windows-gnu";    outDir = "x86_64-pc-windows-gnu"; };
          spinner-mac-arm64 = spinnerFor { zigTarget = "aarch64-apple-darwin";     outDir = "aarch64-apple-darwin"; };
        } // runtimes;
        devShells.default = import ./nix/devshell.nix { inherit pkgs lib spaTools; };
      });
}
