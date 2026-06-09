# Development shell: the full NixOS build leg toolchain + the env that makes
# generic/prebuilt binaries and electron-builder behave on NixOS.
{ pkgs, lib, spaTools ? [ ] }:
let
  # Toolchain to build Open-WebUI (node + python), assemble the relocatable
  # runtime (uv), build the Electron app + tailwind, and run the bundler.
  buildTools = with pkgs; [
    # node (Open-WebUI requires <=22.x) + electron app + tailwindcss
    nodejs_22
    tailwindcss_3
    # python (Open-WebUI requires >=3.11,<3.13) + uv resolver/venv
    python312
    uv
    # archive + fetch + json
    jq curl cacert zstd gnutar unzip gzip pigz git gnused coreutils which
    # electron-builder linux packaging (AppImage)
    fakeroot dpkg fuse
    p7zip     # system 7za so electron-builder skips its non-NixOS bundled one
    patchelf  # repoint electron-builder's prebuilt helpers at the nix loader
    # cross-target packaging from NixOS
    rcodesign                 # Apple code signing from Linux (mac target)
    # electron-builder's win step runs the 32-bit rcedit-ia32.exe (sets exe
    # version strings). The new wow64 wine (wineWow64Packages) can't run 32-bit
    # PEs and segfaults; the classic multilib WoW build does. Deprecation warning
    # is upstream preferring wow64 — ignore it, we need real 32-bit support.
    wineWowPackages.stable
    osslsigncode              # Authenticode signing for the windows .exe
    nsis                      # windows installer
    # build graph: ninja runs every step with dependency tracking (xtask emits
    # build.ninja; the Makefile drives it via `nix run .#xtask -- build`).
    ninja
    # USB image: FAT32 only (mtools, no root) — artifacts stay < 4 GiB
    mtools dosfstools zip
    # component images: squashfs (linux, mounted via bundled squashfuse) and
    # HFS+ .dmg (mac, mounted via hdiutil) built from NixOS
    squashfsTools hfsprogs
    # VM test (ubuntu): qemu + cloud-utils fallback (incus is used from the host)
    qemu cloud-utils
    # NixOS launch test (make test-nixos runs the bundle headless under Xvfb)
    xvfb-run
  ];

  # Generic prebuilt binaries (electron-builder helpers; Open-WebUI native wheels;
  # ollama runners) expect FHS libs absent on NixOS. Exposed via NIX_LD (build
  # helpers) and, for runtime children, PLANAI_CHILD_LD_LIBRARY_PATH.
  ldLibs = with pkgs; [
    stdenv.cc.cc.lib            # libstdc++, libgcc_s, libgomp
    zlib glib fuse libGL
    libffi openssl expat bzip2 xz
    stdenv.cc.libc              # libm, libpthread, libdl, libc
  ];
in
pkgs.mkShell {
  # buildTools + the Dioxus SPA toolchain (rust+wasm32, dx, wasm-bindgen-cli,
  # binaryen, lld) so `nix develop` can build launcher/spa-src via scripts/build-spa.sh.
  packages = buildTools ++ spaTools;
  shellHook = ''
    export ELECTRON_OVERRIDE_DIST_PATH="${pkgs.electron}/libexec/electron"
    export ELECTRON_SKIP_BINARY_DOWNLOAD=1
    export PLAYWRIGHT_BROWSERS_PATH=0
    export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"

    # Let electron-builder's prebuilt helpers run via the nix-ld stub.
    export NIX_LD="$(cat ${pkgs.stdenv.cc}/nix-support/dynamic-linker)"
    export NIX_LD_LIBRARY_PATH="${lib.makeLibraryPath ldLibs}"
    export USE_SYSTEM_7ZA=true

    # marker the Makefile guards on (see Makefile)
    export PLANAI_DEVSHELL=1

    if [ -f .gitmodules ] && [ ! -e third_party/plan-ai-design/assets/input.css ]; then
      echo "==> initialising plan-ai-design submodule"
      git submodule update --init --recursive third_party/plan-ai-design || true
    fi
    if [ -f usb.lock ]; then
      echo "plan-ai-usb-minimal — pinned versions:"
      jq -r '"  ollama     \(.ollama.version)\n  open-webui \(.openwebui.version)\n  python     \(.python)"' usb.lock
    fi
  '';
}
