# Shared dev-leg definition: the package set + environment that make the build
# toolchain (and generic/prebuilt binaries) behave on NixOS. Consumed by BOTH
# the interactive `nix develop` shell (nix/devshell.nix) and the bundled Docker
# image (nix/docker.nix), so the container reproduces the devshell 1:1.
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
    # electron-builder linux packaging (dir target)
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
    # push the devshell Docker image to a registry (make docker-push) without a
    # docker daemon — reads the dockerTools archive, writes to docker://<repo>.
    skopeo
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

  packages = buildTools ++ spaTools;

  # Environment shared by the shell (exported in its shellHook) and the image
  # (baked into the layer's Env). All values are plain strings referencing nix
  # store paths whose closures the consumers pull in.
  env = {
    ELECTRON_OVERRIDE_DIST_PATH = "${pkgs.electron}/libexec/electron";
    ELECTRON_SKIP_BINARY_DOWNLOAD = "1";
    PLAYWRIGHT_BROWSERS_PATH = "0";
    SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    # Let electron-builder's prebuilt helpers run via the nix-ld stub.
    NIX_LD = lib.fileContents "${pkgs.stdenv.cc}/nix-support/dynamic-linker";
    NIX_LD_LIBRARY_PATH = lib.makeLibraryPath ldLibs;
    USE_SYSTEM_7ZA = "true";
    # marker the Makefile guards on (see Makefile)
    PLANAI_DEVSHELL = "1";
  };
in
{
  inherit buildTools ldLibs packages env;
}
