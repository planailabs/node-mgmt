# Reusable dev-environment base: an interactive `nix develop` shell and the same
# environment bundled as a Docker image (dockerTools, nix-inside, no Dockerfile),
# both driven by a consumer-supplied package set + env. Extracted from the plan.ai
# devshell/docker so any product reuses it; the consumer passes its own packages, env,
# project shellHook, and any extra packages/image contents.
{ pkgs, lib }:
rec {
  # `export NAME=value` lines for an env attrset, deterministic order.
  mkExports = env: lib.concatStringsSep "\n"
    (lib.mapAttrsToList (n: v: "export ${n}=${lib.escapeShellArg v}") env);

  # An interactive dev shell. `packages` + `env` come from the consumer's dev-env;
  # `shellHook` is the project's extra setup (submodule init, banners…); `extraPackages`
  # lets a downstream add tools without forking the base.
  mkDevShell = { packages, env ? { }, shellHook ? "", extraPackages ? [ ] }:
    pkgs.mkShell {
      packages = packages ++ extraPackages;
      shellHook = ''
        ${mkExports env}
        ${shellHook}
      '';
    };

  # The dev environment as a Docker image: ships a working `nix` (flakes on, store DB
  # seeded) so a flake-orchestrated `make` runs in-container, plus a nix-ld FHS shim so
  # the prebuilt/patched helpers resolve their interpreter on the plain-glibc rootfs.
  # `name`/`packages`/`env` from the consumer; `extraContents` for downstream additions.
  mkDevImage = { name, packages, env ? { }, extraContents ? [ ] }:
    let
      nixLd = "${pkgs.nix-ld}/libexec/nix-ld";
      nixLdFhs = pkgs.runCommand "nix-ld-fhs" { } ''
        mkdir -p "$out/lib64" "$out/lib"
        ln -s ${nixLd} "$out/lib64/ld-linux-x86-64.so.2"
        ln -s ${nixLd} "$out/lib/ld-linux-x86-64.so.2"
      '';
      nixConf = pkgs.writeTextDir "etc/nix/nix.conf" ''
        experimental-features = nix-command flakes
        build-users-group =
        sandbox = false
      '';
      imageTools = with pkgs; [
        bashInteractive gnumake gawk findutils gnugrep diffutils patch
        nix cacert dockerTools.fakeNss
      ];
      envList = lib.mapAttrsToList (n: v: "${n}=${v}") env;
    in
    pkgs.dockerTools.buildLayeredImageWithNixDb {
      inherit name;
      tag = "latest";
      # lib.unique: dev package sets often fold duplicate store paths (e.g. nodejs,
      # tailwind via two tool groups); dockerTools' gcroot creation collides on dupes.
      contents = lib.unique (packages ++ imageTools ++ extraContents ++ [ nixLdFhs nixConf ]);
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
    };
}
