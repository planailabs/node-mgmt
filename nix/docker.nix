# The devshell, bundled as a Docker image (built on NixOS via dockerTools — no
# Dockerfile, no docker daemon). Reproduces nix/devshell.nix 1:1 — same package
# closure, same env — AND ships a working `nix` (flakes enabled, store DB
# seeded) so the whole `make` pipeline runs inside the container: the Makefile
# orchestrates every step through `nix run .#xtask` / `nix build .#<attr>`.
#
#   nix build .#devshell-image
#   docker load < result
#   docker run --rm -it -v "$PWD:/workspace" plan-ai-usb-devshell:latest \
#     make image PLATFORMS=linux-x64
#
# The container's /nix/store ships the devshell closure; building the flake's
# own components (runtimes, launchers, …) re-evaluates the flake and fetches /
# builds on demand, so the container needs network (substituters) at run time.
# The PLANAI_DEVSHELL marker is baked in, so the Makefile's devshell guard
# passes without `nix develop`.
{ pkgs, lib, devEnv }:
let
  # The image is a plain glibc rootfs, NOT NixOS — so the FHS interpreter path
  # the prebuilt helpers (electron-builder rcedit/7za, ollama runners, OW native
  # wheels) are patched to use does not exist. Drop nix-ld at those paths so the
  # same NIX_LD / NIX_LD_LIBRARY_PATH indirection the devshell relies on resolves
  # the libs here too.
  nixLd = "${pkgs.nix-ld}/libexec/nix-ld";
  nixLdFhs = pkgs.runCommand "nix-ld-fhs" { } ''
    mkdir -p "$out/lib64" "$out/lib"
    ln -s ${nixLd} "$out/lib64/ld-linux-x86-64.so.2"
    ln -s ${nixLd} "$out/lib/ld-linux-x86-64.so.2"
  '';

  # nix.conf for in-container builds: flakes on, and the sandbox off because an
  # unprivileged `docker run` lacks the user-namespace caps nix's build sandbox
  # needs (the alternative is requiring --privileged). Substituters left at the
  # defaults so the flake's components fetch from cache.nixos.org at run time.
  nixConf = pkgs.writeTextDir "etc/nix/nix.conf" ''
    experimental-features = nix-command flakes
    build-users-group =
    sandbox = false
  '';

  # gnumake is provided implicitly by stdenv inside `nix develop`; the image has
  # no such wrapper, so add it (and the basic shell utils) explicitly. nix +
  # cacert + git make the flake-orchestrated build pipeline runnable in-container;
  # fakeNss gives the root user/group lookups nix and git expect.
  imageTools = with pkgs; [
    # stdenv implicitly puts these on PATH inside `nix develop`; the image has no
    # such wrapper, so add the ones the build scripts/Makefile actually call.
    bashInteractive gnumake gawk findutils gnugrep diffutils patch
    nix cacert dockerTools.fakeNss
  ];

  envList = lib.mapAttrsToList (n: v: "${n}=${v}") devEnv.env;
in
pkgs.dockerTools.buildLayeredImageWithNixDb {
  name = "plan-ai-usb-devshell";
  tag = "latest";
  # lib.unique: devEnv.packages folds buildTools + spaTools, which both carry
  # nodejs_22 / tailwindcss_3. mkShell tolerates the dupes; dockerTools' gcroot
  # creation collides on them, so collapse to distinct store paths here.
  contents = lib.unique (devEnv.packages ++ imageTools ++ [ nixLdFhs nixConf ]);
  # Writable scratch dirs + the nix state tree the seeded DB lives under.
  extraCommands = ''
    mkdir -p tmp root workspace nix/var/nix
    chmod 1777 tmp
  '';
  config = {
    Cmd = [ "${pkgs.bashInteractive}/bin/bash" ];
    WorkingDir = "/workspace";
    Env = envList ++ [
      "PATH=/bin"
      "HOME=/root"
      "USER=root"
      "NIX_PAGER="
      "GIT_SSL_CAINFO=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      "SSL_CERT_DIR=${pkgs.cacert}/etc/ssl/certs"
    ];
  };
}
