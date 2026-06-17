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
    # Include git submodules in the flake source. The SPA build needs
    # third_party/plan-ai-design (the shared Dioxus component library), and the
    # launcher needs third_party/mac-mgmt (mac-mgmt-services — the cross-platform
    # process supervisor its control plane drives); both are git submodules, pinned by
    # commit, that would otherwise be excluded from the flake source tree. Vendoring
    # mac-mgmt as a submodule (rather than a flake input) lets the launcher build with
    # plain `cargo build` outside the flake too. Bump the submodule after landing
    # changes in mac-mgmt.
    self.submodules = true;
  };

  # Thin entrypoint — the real definitions live in nix/:
  #   nix/devshell.nix  the dev shell (toolchain + NixOS env)
  #   nix/vendor.nix    layer 1 download FODs + layer 2 no-fixup ollama repack
  #   nix/runtime.nix   layer 3 portable open-webui runtime (wheels-FOD + vanilla install)
  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
        lib = pkgs.lib;

        usbLock = builtins.fromJSON (builtins.readFile ./usb.lock);

        # rust toolchain with the cross-target std libs the launcher needs
        rustToolchain = pkgs.rust-bin.stable.latest.minimal.override {
          targets = [ "x86_64-pc-windows-gnu" "aarch64-apple-darwin"
                      "x86_64-unknown-linux-gnu" "x86_64-unknown-linux-musl"
                      "aarch64-unknown-linux-gnu" "aarch64-unknown-linux-musl" ];
        };
        # Full stable toolchain + wasm32 for the Dioxus SPA (dx/cargo need a full
        # rustc for wasm32-unknown-unknown).
        wasmToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };
        wasmRustPlatform = pkgs.makeRustPlatform { cargo = wasmToolchain; rustc = wasmToolchain; };
        # Rust platform over the cross-target toolchain (rustToolchain carries the
        # win/mac/linux std libs). Used to build the usb daemon for win/mac via
        # cargo-zigbuild while letting buildRustPackage's cargoSetupHook vendor the
        # registry + git deps (mac-mgmt pins a dioxus/swiftide fork) offline.
        crossRustPlatform = pkgs.makeRustPlatform { cargo = rustToolchain; rustc = rustToolchain; };

        # Scoped sources (lib.fileset): each derivation pulls only the files it
        # actually reads, so unrelated repo changes don't trigger rebuilds.
        # third_party/mac-mgmt always travels whole where needed — its workspace
        # root requires every member manifest, so it can't be sliced further.
        spaSrc = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./launcher/spa-src           # the crate
            ./third_party/plan-ai-design # path dep
            ./crates/control-api         # path dep
            ./third_party/loader/crates/loader-manifest # control-api's update-DTO dep
            ./third_party/mac-mgmt       # config-ui + common path deps
          ];
        };
        # xtask + its engine/manifest now live in the loader submodule; build it
        # from there (self.submodules brings the tree into the flake source).
        xtaskSrc = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./third_party/loader/crates/xtask
            ./third_party/loader/crates/loader-engine
            ./third_party/loader/crates/loader-manifest
          ];
        };
        # the launcher crate without spa-src (the SPA is its own derivation,
        # passed in via PLANAI_SPA_DIST — see launcherFor)
        launcherSrc = lib.fileset.toSource {
          root = ./launcher;
          fileset = lib.fileset.difference ./launcher ./launcher/spa-src;
        };

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
          # scoped source: spa-src + its path deps (plan-ai-design,
          # control-api, and the mac-mgmt submodule for config-ui/common).
          src = spaSrc;
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
          src = xtaskSrc;
          cargoRoot = "third_party/loader/crates/xtask";
          buildAndTestSubdir = "third_party/loader/crates/xtask";
          cargoLock.lockFile = ./third_party/loader/crates/xtask/Cargo.lock;
          doCheck = false;
          meta.mainProgram = "xtask";
        };
        # registry deps for the launcher's Cargo.lock (tokio, interprocess, …),
        # vendored offline. The mac-mgmt-services path dep is supplied separately
        # (the third_party/mac-mgmt submodule, copied beside src/ in the build).
        launcherVendor = pkgs.rustPlatform.importCargoLock {
          lockFile = ./launcher/Cargo.lock;
        };
        # native launcher cross-compiled via cargo-zigbuild. It now carries deps
        # (the control plane), so the build vendors crates.io (launcherVendor) and
        # copies mac-mgmt-services from the third_party/mac-mgmt submodule.
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
              else if lib.hasInfix "aarch64" zigTarget then
                spinnerFor { zigTarget = "aarch64-unknown-linux-gnu"; outDir = "aarch64-unknown-linux-gnu"; }
              else if lib.hasInfix "x86_64" zigTarget then
                spinnerFor { zigTarget = "x86_64-unknown-linux-gnu"; outDir = "x86_64-unknown-linux-gnu"; }
              else
                throw "launcherFor: no spinner mapping for zigTarget '${zigTarget}'";
            spinnerBin = "${spinnerPkg}/${if lib.hasInfix "windows" zigTarget then "plan-ai-spinner.exe" else "plan-ai-spinner"}";
          in
          pkgs.runCommand "plan-ai-launcher-${outDir}"
            ({
              nativeBuildInputs = [ rustToolchain pkgs.cargo-zigbuild pkgs.zig ]
                ++ lib.optionals (lib.hasInfix "apple-darwin" zigTarget) [ pkgs.python3 pkgs.rcodesign ];
              # build.rs embeds these into the linux launcher (mounts squashfs itself,
              # like the AppImage runtime); ignored for win/mac targets.
              PLANAI_SQUASHFUSE_LL = "${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll";
              PLANAI_UNSQUASHFS = "${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs";
              # the splash spinner, embedded into the launcher for every target.
              PLANAI_SPINNER_BIN = spinnerBin;
              # the built SPA assets, embedded by serve.rs's rust-embed via
              # build.rs (the launcher src carries no spa/ dir).
              PLANAI_SPA_DIST = "${spa}";
              # The optional-feature catalog from loader.toml ([[feature]] name+default),
              # baked into loader-manifest's KNOWN_FEATURES by its build.rs. Single source
              # of truth — edit loader.toml, not the crate.
              PLANAI_FEATURES = builtins.concatStringsSep " "
                (map (f: "${f.name}=${if (f.default or false) then "1" else "0"}") (loaderToml.feature or [ ]));
              # The fallback update URL baked into loader-manifest's DEFAULT_UPDATE_URL (used
              # only when a drive has no local manifest) — sourced from loader.toml, not hardcoded
              # in the loader engine.
              PLANAI_UPDATE_URL = loaderToml.manifest.update_url or "";
            } // lib.optionalAttrs (lib.hasInfix "linux" zigTarget) {
              # Static bubblewrap, embedded into the linux launcher: on NixOS it sets up
              # the OUTER namespace that binds/overlays the FHS-closure squashfs over
              # /nix/store (no `nix-store --import`, so no trusted-user requirement),
              # then runs the buildFHSEnv wrapper inside it. Static so it runs with no
              # store deps before the store is provided.
              PLANAI_BWRAP_BIN = "${pkgs.pkgsStatic.bubblewrap}/bin/bwrap";
            } // lib.optionalAttrs (lib.hasInfix "apple-darwin" zigTarget) {
              # Cocoa headers/frameworks for notify-rust's mac-notification-sys.
              SDKROOT = macosx-sdk;
            })
            ''
              export HOME="$TMPDIR" CARGO_HOME="$TMPDIR/cargo" XDG_CACHE_HOME="$TMPDIR/cache"
              # path deps copied as siblings of src/ so Cargo's relative paths resolve:
              #   ../crates/* and ../third_party/mac-mgmt/mac-mgmt-services (a git
              #   submodule — self.submodules brings it into the flake source; only that
              #   self-contained crate subdir is needed).
              cp -r ${./crates} crates && chmod -R u+w crates
              mkdir -p third_party/mac-mgmt
              cp -r ${./third_party/mac-mgmt}/mac-mgmt-services third_party/mac-mgmt/mac-mgmt-services
              # the launcher's update-manifest + runtime-substrate deps now live in
              # the loader submodule.
              mkdir -p third_party/loader/crates
              cp -r ${./third_party/loader/crates/loader-manifest} third_party/loader/crates/loader-manifest
              cp -r ${./third_party/loader/crates/loader-core} third_party/loader/crates/loader-core
              chmod -R u+w third_party
              cp -r ${launcherSrc}/. src && chmod -R u+w src && cd src
              # crates.io deps from the vendored cargo lock
              mkdir -p .cargo
              printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s"\n' "${launcherVendor}" > .cargo/config.toml
              cargo zigbuild --release --offline --target ${zigTarget}
              mkdir -p "$out"
              for b in plan-ai plan-ai.exe; do
                if [ -f "target/${outDir}/release/$b" ]; then cp "target/${outDir}/release/$b" "$out/"; fi
              done
              ${lib.optionalString (lib.hasInfix "apple-darwin" zigTarget) ''
                # Same dyld duplicate-dylib hazard as the spinner (zig + objc crates):
                # repoint any duplicate to its alias and re-sign ad-hoc. No-op when the
                # binary has no duplicate, so it's safe to run unconditionally on mac.
                chmod +w "$out/plan-ai"
                python3 ${./scripts/macho-dedupe-dylibs.py} "$out/plan-ai"
                rcodesign sign "$out/plan-ai" "$out/plan-ai"
              ''}
            '';

        # registry deps for the spinner crate's Cargo.lock (eframe + its tree).
        spinnerVendor = pkgs.rustPlatform.importCargoLock {
          lockFile = ./third_party/loader/crates/spinner/Cargo.lock;
        };
        # The native splash spinner (eframe/glow), cross-built like the launcher via
        # cargo-zigbuild. Unlike the launcher it CANNOT be static-musl (a GUI needs a
        # dynamic loader), so linux targets gnu. glow/winit dlopen the GL + windowing
        # libs at runtime, so the binary carries no nix-store paths and resolves the
        # TARGET machine's system libs (libGL/libX11/…). mac links AppKit/OpenGL etc.
        # from the Apple SDK via SDKROOT (same mechanism the launcher uses for Cocoa).
        # Splash colour scheme from loader.toml [spinner] — baked into the spinner at
        # build time (the spinner's build.rs reads these). Empty → its plan.ai defaults.
        spinnerColors = let s = (builtins.fromTOML (builtins.readFile ./loader.toml)).spinner or { }; in {
          PLANAI_SPINNER_CANVAS = s.canvas or "";
          PLANAI_SPINNER_BRAND = s.brand or "";
          PLANAI_SPINNER_TEXT = s.text or "";
          PLANAI_SPINNER_TRACK = s.track or "";
        };
        spinnerFor = { zigTarget, outDir }:
          pkgs.runCommand "plan-ai-spinner-${outDir}"
            ({
              nativeBuildInputs = [ rustToolchain pkgs.cargo-zigbuild pkgs.zig ]
                ++ lib.optionals (lib.hasInfix "apple-darwin" zigTarget) [ pkgs.python3 pkgs.rcodesign ];
            } // spinnerColors // lib.optionalAttrs (lib.hasInfix "apple-darwin" zigTarget) {
              SDKROOT = macosx-sdk;
            })
            ''
              export HOME="$TMPDIR" CARGO_HOME="$TMPDIR/cargo" XDG_CACHE_HOME="$TMPDIR/cache"
              cp -r ${./third_party/loader/crates/spinner}/. src && chmod -R u+w src && cd src
              mkdir -p .cargo
              printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "%s"\n' "${spinnerVendor}" > .cargo/config.toml
              cargo zigbuild --release --offline --target ${zigTarget}
              mkdir -p "$out"
              for b in plan-ai-spinner plan-ai-spinner.exe; do
                if [ -f "target/${outDir}/release/$b" ]; then cp "target/${outDir}/release/$b" "$out/"; fi
              done
              ${lib.optionalString (lib.hasInfix "apple-darwin" zigTarget) ''
                # zig links libobjc.A.dylib twice -> modern dyld SIGABRTs before main()
                # ("duplicate linked dylib"), so the GUI spinner never starts on macOS.
                # Repoint the duplicate to its symlink alias (ordinals preserved) and
                # re-sign ad-hoc (the edit voids zig's linker signature).
                chmod +w "$out/plan-ai-spinner"
                python3 ${./scripts/macho-dedupe-dylibs.py} "$out/plan-ai-spinner"
                rcodesign sign "$out/plan-ai-spinner" "$out/plan-ai-spinner"
              ''}
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
        # MSVC C++ redistributable DLLs (vcruntime140*.dll, …), a standalone target
        # shared-pinned with the python runtime. Bundled beside the msvc-linked
        # llmfit.exe so it finds VCRUNTIME140.dll on a machine without the VC++ redist.
        msvcDlls = (import ./third_party/loader/nix/loader/msvc-runtime.nix { inherit pkgs; }).dlls;
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

        # the vendored hermes-agent source tree (FOD tarball → plain tree); the
        # hermes component + its web build consume it.
        hermesSrcTree = pkgs.runCommand "hermes-agent-src-${vendorLock.hermes.tag}"
          { src = pkgs.fetchurl { inherit (vendorLock.hermes) url sha256; }; }
          ''mkdir -p $out && tar -xzf "$src" -C $out --strip-components=1'';

        # portable hermes component per target (same wheels-FOD + vanilla install
        # shape as runtimeFor; see nix/hermes.nix)
        hermesFor = { target, triple, lockFile }:
          import ./nix/hermes.nix {
            inherit pkgs lib system triple;
            pyVersion = vendorLock.pbs.python;
            pbsArchive = pbsFor target;
            wheelsLock = builtins.fromJSON (builtins.readFile lockFile);
            hermesSrc = hermesSrcTree;
            hermesTag = vendorLock.hermes.tag;
          };
        hermesComponents = {
          hermes-linux-x64 = hermesFor {
            target = "linux-x64"; triple = "x86_64-unknown-linux-gnu";
            lockFile = ./hermes/wheels-linux-x64.lock.json;
          };
          hermes-linux-arm64 = hermesFor {
            target = "linux-arm64"; triple = "aarch64-unknown-linux-gnu";
            lockFile = ./hermes/wheels-linux-arm64.lock.json;
          };
          hermes-win-x64 = hermesFor {
            target = "win-x64"; triple = "x86_64-pc-windows-msvc";
            lockFile = ./hermes/wheels-win-x64.lock.json;
          };
          hermes-mac-arm64 = hermesFor {
            target = "mac-arm64"; triple = "aarch64-apple-darwin";
            lockFile = ./hermes/wheels-mac-arm64.lock.json;
          };
        };

        # hermes-webui: the lightweight three-panel web UI for the hermes agent
        # (optional "hermes" feature, alongside the dashboard). Pure python +
        # static sources — no venv of its own: the usbd service runs it with the
        # hermes component's portable python (HERMES_WEBUI_PYTHON) and points
        # HERMES_WEBUI_AGENT_DIR at that component's site-packages. One shared
        # component for all platforms (like ow-assets), store-independent by
        # construction (plain file copies).
        hermesWebuiSrcTree = pkgs.runCommand "hermes-webui-src-${vendorLock."hermes-webui".tag}"
          { src = pkgs.fetchurl { inherit (vendorLock."hermes-webui") url sha256; }; }
          ''mkdir -p $out && tar -xzf "$src" -C $out --strip-components=1'';
        hermesWebuiComponent = pkgs.runCommand "plan-ai-hermes-webui" { } ''
          mkdir -p "$out"
          cp -r ${hermesWebuiSrcTree}/api ${hermesWebuiSrcTree}/static ${hermesWebuiSrcTree}/scripts "$out/"
          install -m 0644 ${hermesWebuiSrcTree}/bootstrap.py ${hermesWebuiSrcTree}/server.py \
            ${hermesWebuiSrcTree}/mcp_server.py ${hermesWebuiSrcTree}/requirements.txt "$out/"
          echo '{ "version": "${vendorLock."hermes-webui".tag}" }' > "$out/hermes-webui.json"
          find "$out" -name '__pycache__' -type d -prune -exec rm -rf {} + || true
        '';

        # uv cross-resolution triples per distributable target (mac-x64 dropped —
        # arm64-only macOS wheels). nixos-x64 reuses the linux-x64 runtime.
        runtimes = {
          runtime-linux-x64 = runtimeFor {
            target = "linux-x64"; triple = "x86_64-unknown-linux-gnu";
            lockFile = ./runtime/wheels-linux-x64.lock.json;
          };
          runtime-linux-arm64 = runtimeFor {
            target = "linux-arm64"; triple = "aarch64-unknown-linux-gnu";
            lockFile = ./runtime/wheels-linux-arm64.lock.json;
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
        # Built per linux arch: the FHS closure is the target machine's /nix/store
        # paths (glibc, gtk3, …), so an aarch64 NixOS needs an aarch64-linux closure.
        # We have native aarch64 builders, so each just builds on its own system.
        # The buildFHSEnv builder lives in the shared loader nix lib; the package
        # set is DATA from loader.toml [fhs] (so any product supplies its own libs).
        loaderFhs = import ./third_party/loader/nix/loader/fhs.nix { inherit nixpkgs; };
        loaderToml = builtins.fromTOML (builtins.readFile ./loader.toml);
        mkNixosFhs = fhsSystem: loaderFhs {
          system = fhsSystem;
          name = loaderToml.fhs.name;
          runScript = loaderToml.fhs.run_script;
          targetPkgNames = loaderToml.fhs.target_pkgs;
        };
        nixosFhs = mkNixosFhs "x86_64-linux";
        nixosFhs-arm64 = mkNixosFhs "aarch64-linux";

        linuxMountTools = pkgs.runCommand "plan-ai-linux-mount-tools" { } ''
          mkdir -p "$out/bin"
          cp ${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll "$out/bin/squashfuse_ll"
          cp ${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs  "$out/bin/unsquashfs"
          chmod +x "$out/bin/"*
        '';

        # The plan.ai USB daemon (`usbd`): the reduced phone-home control
        # plane the launcher spawns (it owns the supervisor + spawn-from-mount
        # services + heartbeat/probe/relay/config-sync). Built NATIVELY for the
        # host — the linux `make test-nixos` runs it. The daemon has a large
        # dep tree (libp2p/memvault/russh), so the first build is slow.
        # memvault's build.rs embeds a precompiled wasm "extract guest"; building it
        # at daemon-build time fails in the sandbox (needs the wasm target + offline
        # cargo). mac-mgmt's own build sidesteps this by passing a prebuilt wasm via
        # MEMVAULT_EXTRACT_GUEST_WASM — replicate that here (same as its overlay.nix).
        memvaultExtractGuestWasm = wasmRustPlatform.buildRustPackage {
          pname = "memvault-extract-guest-wasm";
          version = "0.1.0";
          src = ./third_party/mac-mgmt/memvault;
          cargoLock = {
            lockFile = ./third_party/mac-mgmt/memvault/Cargo.lock;
            outputHashes = import ./third_party/mac-mgmt/memvault/extra-hashes.nix;
          };
          nativeBuildInputs = [ pkgs.lld ];
          cargoBuildFlags = [ "-p" "memvault-extract-guest" "--target" "wasm32-unknown-unknown" ];
          doCheck = false;
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp target/wasm32-unknown-unknown/release/memvault_extract_guest.wasm \
              $out/memvault_extract_guest.wasm
            runHook postInstall
          '';
        };
        # The memvault web UI client: memvault-web's WASM bundle + static assets
        # (tailwind comes from the crate's build.rs), dx-built like the SPA
        # above. The output is dx's `public/` directory, shipped ADJACENT to the
        # usbd binary in every usbd component — dioxus-server serves
        # `<exe dir>/public` when DIOXUS_PUBLIC_PATH is unset, so the in-process
        # memvault web app (usbd's run.rs::maybe_serve_memvault) finds it there.
        # Pure wasm + static files, so ONE build feeds all four usbd targets.
        # `--no-default-features --features web` keeps the server's native deps
        # (tokio/mio) out of the wasm build — the same @client split as
        # mac-mgmt's build-memvault.sh.
        #
        # CRITICAL: src is the SAME stitched usbdSrc tree the usbd binary is
        # built from. mac-mgmt gets client/server consistency by dx-building
        # both legs in one invocation from one checkout; usbd's server leg is
        # cargo/zigbuild-built, so we replicate the part that actually matters:
        # the dioxus-fullstack macro hashes `CARGO_MANIFEST_DIR:module_path`
        # into every server-fn endpoint (/api/<fn><xxh64>), so the client and
        # the server must compile memvault-web from the IDENTICAL /build path
        # or every endpoint 404s under hydration.
        memvaultWebClient = wasmRustPlatform.buildRustPackage {
          pname = "memvault-web-client";
          version = "0.1.0";
          src = usbdSrc;
          cargoRoot = "third_party/mac-mgmt/memvault";
          cargoLock = {
            lockFile = ./third_party/mac-mgmt/memvault/Cargo.lock;
            outputHashes = import ./third_party/mac-mgmt/memvault/extra-hashes.nix;
          };
          nativeBuildInputs = spaTools;
          buildPhase = ''
            runHook preBuild
            export HOME="$TMPDIR" CARGO_NET_OFFLINE=true
            cd third_party/mac-mgmt/memvault/crates/memvault-web
            dx build --platform web --release --no-default-features --features web
            cd ../../../../..
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp -r third_party/mac-mgmt/memvault/target/dx/memvault-web/release/web/public/. "$out/"
            # usbd is cargo-built (not dx-built), so the manganis link section
            # in the server binary still holds placeholders: the SSR'd
            # `document::Stylesheet { asset!("/public/tailwind.css") }` href
            # can't resolve to the hashed filename and 404s. Link the unhashed
            # copy (served from the public root) straight from the index.html
            # shell so the page is styled before — and regardless of — WASM
            # hydration.
            sed -i 's|</head>|<link rel="stylesheet" href="/tailwind.css"></head>|' "$out/index.html"
            runHook postInstall
          '';
          doCheck = false;
        };
        # usbd is its own package (usbd/) on top of the mac-mgmt-agent crate
        # from the submodule, so the build no longer compiles the daemon CLI
        # blob (rocket/MCP servers/plan-ai-cloud/dashboard). The source is
        # stitched: usbd/ beside the FULL third_party/mac-mgmt tree (incl. the
        # nested memvault submodule and the workspace root Cargo.toml — cargo
        # loads that root for the member path deps mac-mgmt-agent/common/
        # mac-mgmt-services, and memvault crates resolve as ../memvault/…).
        # Network parts (heartbeat/relay/sync) are a RUNTIME toggle
        # (USBD_NETWORKED, set by the launcher from the drive's "mgmt" feature).
        usbdSrc = pkgs.runCommand "plan-ai-usbd-src" { } ''
          mkdir -p "$out/third_party/loader/crates" "$out/crates"
          cp -r ${./usbd} "$out/usbd"
          cp -r ${./crates/usb-config} "$out/crates/usb-config"
          # usbd's control-plane HTTP contract, and its loader-manifest update-DTO dep
          cp -r ${./crates/control-api} "$out/crates/control-api"
          cp -r ${./third_party/loader/crates/loader-manifest} "$out/third_party/loader/crates/loader-manifest"
          cp -r ${./third_party/mac-mgmt} "$out/third_party/mac-mgmt"
        '';
        # the dioxus-fork git-dep hashes for usbd/Cargo.lock (memvault-web pins
        # them; [patch.crates-io] lives in usbd/Cargo.toml since patches don't
        # apply transitively). Same hashes as the SPA build.
        usbdGitHashes = import ./usbd/cargo-git-hashes.nix {
          inherit dioxusHash dioxusI18nHash;
        };
        usbd = pkgs.rustPlatform.buildRustPackage {
          pname = "plan-ai-usbd";
          version = "0.1.0";
          src = usbdSrc;
          cargoLock = {
            lockFile = ./usbd/Cargo.lock;
            outputHashes = usbdGitHashes;
          };
          cargoRoot = "usbd";
          buildAndTestSubdir = "usbd";
          doCheck = false;
          cargoBuildFlags = [ "--bin" "usbd" ];
          # nodejs + tailwindcss_3: memvault-web's build.rs runs `npm run
          # tailwind:build` (mac-mgmt-agent's `memvault` feature pulls
          # memvault-web). Matches mac-mgmt's own overlay.nix daemon build inputs.
          nativeBuildInputs = [ pkgs.pkg-config pkgs.protobuf pkgs.nodejs pkgs.tailwindcss_3 ];
          buildInputs = [ pkgs.openssl ];
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          env.GIT_SHA = "usbd-dev";
          env.MEMVAULT_EXTRACT_GUEST_WASM = "${memvaultExtractGuestWasm}/memvault_extract_guest.wasm";
        };
        # `usbd` packaged as a launcher component: the binary at the component
        # ROOT (`mac-mgmt`), so when the launcher mounts `usbd.squashfs` at
        # `<dist>/usbd/` the daemon lands at `PLANAI_RESOURCES/usbd/usbd` —
        # exactly where `usbd::resolve_bin()` looks. The memvault web UI ships
        # beside it as `public/` (dioxus-server's `<exe dir>/public` fallback).
        # Packed into a squashfs by the bundle like the other components
        # (runtime/ollama/ow-assets).
        usbdComponent = pkgs.runCommand "plan-ai-usbd-component" { } ''
          mkdir -p "$out"
          cp ${usbd}/bin/usbd "$out/usbd"
          chmod +x "$out/usbd"
          cp -r ${memvaultWebClient} "$out/public"
        '';
        # Cross-compiled usb daemon for win/mac, built with cargo-zigbuild like the
        # launcher. macOS is Unix so the daemon source compiles unchanged; Windows
        # required a Unix→Windows port in mac-mgmt (signals, PTY/FIFO, file modes,
        # process exec — all `#[cfg(unix)]`-gated). cargoSetupHook (via cargoLock)
        # vendors the registry + git deps offline; cargo-zigbuild supplies the
        # cross C toolchain (ring/quinn build fine). Same feature set as the native
        # `usbd` above.
        usbdFor = { rustTarget, exe, extraEnv ? { } }:
          crossRustPlatform.buildRustPackage ({
            pname = "plan-ai-usbd-${rustTarget}";
            version = "0.1.0";
            src = usbdSrc;
            cargoLock = {
              lockFile = ./usbd/Cargo.lock;
              outputHashes = usbdGitHashes;
            };
            cargoRoot = "usbd";
            doCheck = false;
            # Disable cargo-auditable (on by default in nixpkgs' buildRustPackage):
            # it injects `-Wl,--undefined=AUDITABLE_VERSION_INFO` to retain an embedded
            # SBOM symbol, which zig's COFF (windows) and ELF (linux) linkers reject.
            # The bin is reproducible from the pinned Cargo.lock anyway.
            auditable = false;
            nativeBuildInputs = [
              pkgs.cargo-zigbuild pkgs.zig
              pkgs.pkg-config pkgs.protobuf pkgs.nodejs pkgs.tailwindcss_3
            ] ++ lib.optionals (lib.hasInfix "apple-darwin" rustTarget) [ pkgs.python3 pkgs.rcodesign ];
            buildInputs = [ pkgs.openssl ];
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            MEMVAULT_EXTRACT_GUEST_WASM = "${memvaultExtractGuestWasm}/memvault_extract_guest.wasm";
            env.GIT_SHA = "usbd-dev";
            buildPhase = ''
              runHook preBuild
              export HOME="$TMPDIR" XDG_CACHE_HOME="$TMPDIR/cache"
              export CARGO_TARGET_DIR="$PWD/target"
              cargo zigbuild --release --offline --target ${rustTarget} \
                --manifest-path usbd/Cargo.toml --bin usbd
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p "$out/bin"
              cp "target/${rustTarget}/release/${exe}" "$out/bin/${exe}"
              ${lib.optionalString (lib.hasInfix "apple-darwin" rustTarget) ''
                # zig links libobjc.A.dylib twice -> modern dyld SIGABRTs before main()
                # ("duplicate linked dylib"), so usbd never starts on mac. Repoint the
                # duplicate to its alias and re-sign ad-hoc (the edit voids zig's linker
                # signature). Same fix as the launcher/spinner; no-op without a duplicate.
                chmod +w "$out/bin/${exe}"
                python3 ${./scripts/macho-dedupe-dylibs.py} "$out/bin/${exe}"
                rcodesign sign "$out/bin/${exe}" "$out/bin/${exe}"
              ''}
              runHook postInstall
            '';
          } // extraEnv);
        usbd-win-x64 = usbdFor { rustTarget = "x86_64-pc-windows-gnu"; exe = "usbd.exe"; };
        usbd-mac-arm64 = usbdFor {
          rustTarget = "aarch64-apple-darwin"; exe = "usbd";
          extraEnv = { SDKROOT = macosx-sdk; };
        };
        # linux-arm64: cross-compiled (the native `usbd` above covers the x86_64
        # host). glibc like the native build — the daemon runs inside the launcher's
        # FHS namespace on NixOS, where glibc is present.
        usbd-linux-arm64 = usbdFor { rustTarget = "aarch64-unknown-linux-gnu"; exe = "usbd"; };
        # Cross usbd packaged as launcher components (binary at the component ROOT,
        # like `usbdComponent`), so the launcher finds `<usbd>/mac-mgmt[.exe]`.
        usbdComponent-win-x64 = pkgs.runCommand "plan-ai-usbd-component-win-x64" { } ''
          mkdir -p "$out"
          cp ${usbd-win-x64}/bin/usbd.exe "$out/usbd.exe"
          cp -r ${memvaultWebClient} "$out/public"
        '';
        usbdComponent-mac-arm64 = pkgs.runCommand "plan-ai-usbd-component-mac-arm64" { } ''
          mkdir -p "$out"
          cp ${usbd-mac-arm64}/bin/usbd "$out/usbd"
          chmod +x "$out/usbd"
          cp -r ${memvaultWebClient} "$out/public"
        '';
        usbdComponent-linux-arm64 = pkgs.runCommand "plan-ai-usbd-component-linux-arm64" { } ''
          mkdir -p "$out"
          cp ${usbd-linux-arm64}/bin/usbd "$out/usbd"
          chmod +x "$out/usbd"
          cp -r ${memvaultWebClient} "$out/public"
        '';
        # Shared dev-leg toolchain + env (nix/dev-env.nix), consumed by both the
        # interactive devshell and the bundled Docker image below.
        devEnv = import ./nix/dev-env.nix { inherit pkgs lib spaTools; };
        # The reusable dev-environment base (shell + Docker image) from the loader
        # submodule; this project supplies the package set, env, and project shellHook.
        loaderDev = import ./third_party/loader/nix/loader/devshell.nix { inherit pkgs lib; };
        # Project-specific shell setup: init the build's submodules + print pinned versions.
        projectShellHook = ''
          if [ -f .gitmodules ]; then
            [ -e third_party/plan-ai-design/assets/input.css ] || \
              { echo "==> initialising plan-ai-design submodule"; git submodule update --init --recursive third_party/plan-ai-design || true; }
            [ -e third_party/mac-mgmt/mac-mgmt-services/Cargo.toml ] || \
              { echo "==> initialising mac-mgmt submodule"; git submodule update --init --recursive third_party/mac-mgmt || true; }
            [ -e third_party/loader/crates/loader-engine/Cargo.toml ] || \
              { echo "==> initialising loader submodule"; git submodule update --init --recursive third_party/loader || true; }
          fi
          if [ -f usb.lock ]; then
            echo "plan-ai-usb-minimal — pinned versions:"
            jq -r '"  ollama     \(.ollama.version)\n  open-webui \(.openwebui.version)\n  python     \(.python)"' usb.lock
          fi
        '';
      in {
        packages = {
          inherit (vendorPkgs) vendor ollamaComponents llamacppComponents;
          # The devshell, bundled as a Docker image (built via dockerTools on the
          # NixOS build leg — no Dockerfile/daemon). Same toolchain + env as
          # `nix develop`, so `make` runs unchanged inside the container:
          #   nix build .#devshell-image && docker load < result
          devshell-image = loaderDev.mkDevImage { name = "plan-ai-usb-devshell"; packages = devEnv.packages; env = devEnv.env; };
          inherit linuxMountTools appimageRuntime nixosFhs nixosFhs-arm64 spa macosx-sdk libdmg-hfsplus xtask;
          inherit usbd usbdComponent memvaultExtractGuestWasm memvaultWebClient;
          inherit usbd-win-x64 usbd-mac-arm64 usbd-linux-arm64;
          inherit usbdComponent-win-x64 usbdComponent-mac-arm64 usbdComponent-linux-arm64;
          launcher-win-x64 = launcherFor { zigTarget = "x86_64-pc-windows-gnu"; outDir = "x86_64-pc-windows-gnu"; };
          launcher-mac-arm64 = launcherFor { zigTarget = "aarch64-apple-darwin"; outDir = "aarch64-apple-darwin"; };
          # linux: STATIC musl → zero dynamic-loader deps, so the launcher runs on
          # ANY linux incl. NixOS (whose bare nix-ld stub can't run a glibc FHS
          # binary). It autodetects NixOS at runtime and EXTRACTS components there
          # (the generic squashfs mount/patchelf path the node loader uses).
          launcher-linux-x64 = launcherFor { zigTarget = "x86_64-unknown-linux-musl"; outDir = "x86_64-unknown-linux-musl"; };
          # arm64: same static-musl story as x64 — runs on any aarch64 linux incl. NixOS.
          launcher-linux-arm64 = launcherFor { zigTarget = "aarch64-unknown-linux-musl"; outDir = "aarch64-unknown-linux-musl"; };
          # llmfit prebuilt binaries (bundled beside the launcher; GPU detect + serve).
          # Pins from vendor.lock.json (.llmfit.assets) — regen via gen-vendor-lock.sh.
          llmfit-linux-x64 = llmfitBin (llmfitAsset "x86_64-unknown-linux-musl");
          llmfit-win-x64   = llmfitBin (llmfitAsset "x86_64-pc-windows-msvc");
          llmfit-mac-arm64 = llmfitBin (llmfitAsset "aarch64-apple-darwin");
          # MSVC redist DLLs — shipped beside llmfit.exe (own target, not folded into
          # the llmfit FOD, so the pin/extraction stays independent of the binary).
          msvc-dlls-win-x64 = msvcDlls;
          # native splash spinner (shown while the launcher mounts the runtime).
          # linux=gnu (dynamic; a GUI can't be static-musl like the launcher).
          spinner-linux-x64 = spinnerFor { zigTarget = "x86_64-unknown-linux-gnu"; outDir = "x86_64-unknown-linux-gnu"; };
          spinner-linux-arm64 = spinnerFor { zigTarget = "aarch64-unknown-linux-gnu"; outDir = "aarch64-unknown-linux-gnu"; };
          spinner-win-x64   = spinnerFor { zigTarget = "x86_64-pc-windows-gnu";    outDir = "x86_64-pc-windows-gnu"; };
          spinner-mac-arm64 = spinnerFor { zigTarget = "aarch64-apple-darwin";     outDir = "aarch64-apple-darwin"; };
          # Pinned userspace FAT32 tooling for make-usb-image.sh. The image is packed
          # OUTSIDE the nix store (mkfs.vfat + mcopy run against the on-disk drive-root)
          # so the multi-GB drive-root — launchers + components + seeded models — is
          # never `nix store add-path`'d into /nix/store. Only these tools are pinned.
          usb-image-tools = pkgs.buildEnv {
            name = "usb-image-tools";
            # unzip: the image carries Windows components UNPACKED, so make-usb-image.sh
            # expands the per-component .zip into its target folder + removes the zip.
            paths = [ pkgs.mtools pkgs.dosfstools pkgs.coreutils pkgs.findutils pkgs.unzip ];
          };
          hermes-webui = hermesWebuiComponent;
        } // runtimes // hermesComponents
          # llmfit ships an aarch64-linux-musl prebuilt only if upstream released one;
          # add the attr only when the asset is in vendor.lock.json so a missing arm64
          # binary doesn't poison flake eval (the bundle tolerates its absence).
          // lib.optionalAttrs (builtins.any (a: a.target == "aarch64-unknown-linux-musl") vendorLock.llmfit.assets) {
            llmfit-linux-arm64 = llmfitBin (llmfitAsset "aarch64-unknown-linux-musl");
          };
        devShells.default = loaderDev.mkDevShell { packages = devEnv.packages; env = devEnv.env; shellHook = projectShellHook; };
        # Windows cross-build harness for the usb daemon (`mac-mgmt usbd`). Exposes
        # the same toolchain/env the nix `usbd` derivation uses, but interactive +
        # incremental: `nix develop .#usbd-win` then run cargo-zigbuild against the
        # daemon for x86_64-pc-windows-gnu. Used to drive the Unix→Windows port of
        # mac-mgmt to a clean cross-compile before wiring a win usbd component.
        devShells.usbd-win = pkgs.mkShell {
          packages = [
            rustToolchain pkgs.cargo-zigbuild pkgs.zig
            pkgs.pkg-config pkgs.protobuf pkgs.nodejs pkgs.tailwindcss_3 pkgs.openssl
          ];
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          MEMVAULT_EXTRACT_GUEST_WASM = "${memvaultExtractGuestWasm}/memvault_extract_guest.wasm";
          GIT_SHA = "usbd-dev";
          shellHook = ''
            export CARGO_TARGET_DIR="$PWD/dist/.usbd-win-target"
            echo "usbd-win: cargo-zigbuild cross shell. Target dir: $CARGO_TARGET_DIR"
            echo "  cargo zigbuild --release --manifest-path usbd/Cargo.toml \\"
            echo "    --target x86_64-pc-windows-gnu --bin mac-mgmt"
          '';
        };
        # macOS cross-build harness for the usb daemon. macOS is Unix, so the
        # daemon source compiles unchanged; this is purely a cross-compile via
        # cargo-zigbuild + the Apple SDK (same SDKROOT the mac launcher uses).
        devShells.usbd-mac = pkgs.mkShell {
          packages = [
            rustToolchain pkgs.cargo-zigbuild pkgs.zig
            pkgs.pkg-config pkgs.protobuf pkgs.nodejs pkgs.tailwindcss_3
          ];
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          MEMVAULT_EXTRACT_GUEST_WASM = "${memvaultExtractGuestWasm}/memvault_extract_guest.wasm";
          GIT_SHA = "usbd-dev";
          SDKROOT = macosx-sdk;
          shellHook = ''
            export CARGO_TARGET_DIR="$PWD/dist/.usbd-mac-target"
            echo "usbd-mac: cargo-zigbuild cross shell. Target dir: $CARGO_TARGET_DIR"
            echo "  cargo zigbuild --release --manifest-path usbd/Cargo.toml \\"
            echo "    --target aarch64-apple-darwin --bin mac-mgmt"
          '';
        };
      });
}
