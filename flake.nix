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
          targets = [ "x86_64-pc-windows-gnu" "aarch64-apple-darwin" "x86_64-unknown-linux-gnu" ];
        };
        # native launcher (zero deps → builds offline) cross-compiled via cargo-zigbuild.
        launcherFor = rustTarget:
          pkgs.runCommand "plan-ai-launcher-${rustTarget}"
            { nativeBuildInputs = [ rustToolchain pkgs.cargo-zigbuild pkgs.zig ]; }
            ''
              export HOME="$TMPDIR" CARGO_HOME="$TMPDIR/cargo" XDG_CACHE_HOME="$TMPDIR/cache"
              cp -r ${./launcher}/. src && chmod -R u+w src && cd src
              cargo zigbuild --release --offline --target ${rustTarget}
              mkdir -p "$out"
              for b in plan-ai plan-ai.exe; do
                if [ -f "target/${rustTarget}/release/$b" ]; then cp "target/${rustTarget}/release/$b" "$out/"; fi
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
        linuxMountTools = pkgs.runCommand "plan-ai-linux-mount-tools" { } ''
          mkdir -p "$out/bin"
          cp ${pkgs.pkgsStatic.squashfuse}/bin/squashfuse_ll "$out/bin/squashfuse_ll"
          cp ${pkgs.pkgsStatic.squashfsTools}/bin/unsquashfs  "$out/bin/unsquashfs"
          chmod +x "$out/bin/"*
        '';
      in {
        packages = {
          inherit (vendorPkgs) vendor ollamaComponents;
          inherit linuxMountTools;
          launcher-win-x64 = launcherFor "x86_64-pc-windows-gnu";
          launcher-mac-arm64 = launcherFor "aarch64-apple-darwin";
        } // runtimes;
        devShells.default = import ./nix/devshell.nix { inherit pkgs lib; };
      });
}
