{
  description = "plan-ai-usb-minimal — portable offline Ollama + Open-WebUI + Electron stack";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Toolchain needed to build Open-WebUI (node + python), assemble the
        # relocatable runtime (uv), build the Electron app + tailwind, and run
        # the bundler. This is the LINUX build leg; win/mac legs run the same
        # scripts on their own OS (CI matrix).
        buildTools = with pkgs; [
          # node (Open-WebUI requires <=22.x) + electron app + tailwindcss
          nodejs_22

          # python (Open-WebUI requires >=3.11,<3.13) + uv resolver/venv
          python312
          uv

          # archive + fetch + json
          jq
          curl
          cacert
          zstd          # ollama linux assets are .tar.zst
          gnutar
          unzip
          gzip
          git
          gnused
          coreutils
          which

          # electron-builder linux packaging (AppImage)
          fakeroot
          dpkg
          fuse
          p7zip     # system 7za so electron-builder skips its non-NixOS bundled one
          patchelf  # repoint electron-builder's prebuilt helpers at the nix loader
        ];

        # electron-builder ships prebuilt, generic dynamically-linked helper
        # binaries (7za, appimagetool, mksquashfs) that NixOS can't run directly.
        # The nix-ld stub is installed system-wide; point NIX_LD at a real loader
        # + libs so those helpers run.
        ldLibs = with pkgs; [ stdenv.cc.cc.lib zlib glib fuse libGL ];
      in {
        devShells.default = pkgs.mkShell {
          packages = buildTools;

          # electron downloads a prebuilt binary; on NixOS it needs the run
          # path patched. ELECTRON_OVERRIDE_DIST_PATH lets the app reuse the
          # nixpkgs electron if present.
          shellHook = ''
            export ELECTRON_OVERRIDE_DIST_PATH="${pkgs.electron}/libexec/electron"
            export ELECTRON_SKIP_BINARY_DOWNLOAD=1
            export PLAYWRIGHT_BROWSERS_PATH=0
            export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"

            # Let electron-builder's prebuilt helpers run via the nix-ld stub.
            export NIX_LD="$(cat ${pkgs.stdenv.cc}/nix-support/dynamic-linker)"
            export NIX_LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath ldLibs}"
            export USE_SYSTEM_7ZA=true

            # Ensure the design system submodule is present.
            if [ -f .gitmodules ] && [ ! -e third_party/plan-ai-design/assets/input.css ]; then
              echo "==> initialising plan-ai-design submodule"
              git submodule update --init --recursive third_party/plan-ai-design || true
            fi

            if [ -f usb.lock ]; then
              echo "plan-ai-usb-minimal — pinned versions:"
              jq -r '"  ollama     \(.ollama.version)\n  open-webui \(.openwebui.version)\n  python     \(.python)"' usb.lock
            fi
          '';
        };
      });
}
