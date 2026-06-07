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

        # rust toolchain with the cross-target std libs the launcher needs
        rustToolchain = pkgs.rust-bin.stable.latest.minimal.override {
          targets = [ "x86_64-pc-windows-gnu" "aarch64-apple-darwin"
                      "x86_64-unknown-linux-gnu" "x86_64-unknown-linux-musl" ];
        };
        # native launcher (zero deps → builds offline) cross-compiled via cargo-zigbuild.
        # zigTarget may pin a glibc (e.g. ...gnu.2.17) for broad portability; outDir
        # is the bare rust target triple cargo writes under.
        launcherFor = { zigTarget, outDir }:
          pkgs.runCommand "plan-ai-launcher-${outDir}"
            {
              nativeBuildInputs = [ rustToolchain pkgs.cargo-zigbuild pkgs.zig ];
              # build.rs embeds these into the linux launcher (mounts squashfs itself,
              # like the AppImage runtime); ignored for win/mac targets.
              PLANAI_SQUASHFUSE_LL = "${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll";
              PLANAI_UNSQUASHFS = "${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs";
            }
            ''
              export HOME="$TMPDIR" CARGO_HOME="$TMPDIR/cargo" XDG_CACHE_HOME="$TMPDIR/cache"
              cp -r ${./launcher}/. src && chmod -R u+w src && cd src
              cargo zigbuild --release --offline --target ${zigTarget}
              mkdir -p "$out"
              for b in plan-ai plan-ai.exe; do
                if [ -f "target/${outDir}/release/$b" ]; then cp "target/${outDir}/release/$b" "$out/"; fi
              done
            '';

        vendorLock = builtins.fromJSON (builtins.readFile ./vendor.lock.json);
        vendorPkgs = import ./nix/vendor.nix { inherit pkgs lib system vendorLock; };

        # python-build-standalone FOD for a target (the relocatable interpreter)
        pbsFor = target:
          let p = lib.findFirst (x: x.target == target) (throw "no pbs ${target}") vendorLock.pbs.files;
          in pkgs.fetchurl { inherit (p) url sha256; };

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

        linuxMountTools = pkgs.runCommand "plan-ai-linux-mount-tools" { } ''
          mkdir -p "$out/bin"
          cp ${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll "$out/bin/squashfuse_ll"
          cp ${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs  "$out/bin/unsquashfs"
          chmod +x "$out/bin/"*
        '';
      in {
        packages = {
          inherit (vendorPkgs) vendor ollamaComponents;
          inherit linuxMountTools appimageRuntime;
          launcher-win-x64 = launcherFor { zigTarget = "x86_64-pc-windows-gnu"; outDir = "x86_64-pc-windows-gnu"; };
          launcher-mac-arm64 = launcherFor { zigTarget = "aarch64-apple-darwin"; outDir = "aarch64-apple-darwin"; };
          # linux: STATIC musl → zero dynamic-loader deps, so the launcher runs on
          # ANY linux incl. NixOS (whose bare nix-ld stub can't run a glibc FHS
          # binary). It autodetects NixOS at runtime and EXTRACTS components there
          # (the generic squashfs mount/patchelf path the node loader uses).
          launcher-linux-x64 = launcherFor { zigTarget = "x86_64-unknown-linux-musl"; outDir = "x86_64-unknown-linux-musl"; };
        } // runtimes;
        devShells.default = import ./nix/devshell.nix { inherit pkgs lib; };
      });
}
