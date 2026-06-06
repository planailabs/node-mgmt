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

          # cross-target packaging from NixOS
          rcodesign       # Apple code signing from Linux (mac target)
          wineWow64Packages.stable  # electron-builder win build steps (rcedit)
          osslsigncode    # Authenticode signing for the windows .exe
          nsis            # windows installer

          # USB image: FAT32 (mtools, no root) + exFAT (>4GiB files, e.g. the AppImage)
          mtools
          dosfstools
          exfatprogs
          zip
          unzip

          # VM test (ubuntu): incus is used from the host; qemu + cloud-utils
          # provide a fallback path (cloud-localds + qemu-system-x86_64).
          qemu
          cloud-utils
        ];

        # Generic, prebuilt dynamically-linked binaries (electron-builder's
        # helpers; Open-WebUI's native wheels like onnxruntime/chromadb; the
        # ollama runners) expect FHS libs. NixOS has none in /usr/lib, so expose
        # a nix library path used for both NIX_LD (build helpers) and
        # LD_LIBRARY_PATH (runtime children, via scripts/run-nixos.sh).
        ldLibs = with pkgs; [
          stdenv.cc.cc.lib   # libstdc++, libgcc_s, libgomp
          zlib glib fuse libGL
          libffi openssl expat bzip2 xz
          stdenv.cc.libc      # libm, libpthread, libdl, libc
        ];
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

            # marker the Makefile guards on (see Makefile)
            export PLANAI_DEVSHELL=1

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
