# Development shell: the full NixOS build leg toolchain + the env that makes
# generic/prebuilt binaries and electron-builder behave on NixOS.
#
# Package set + env live in nix/dev-env.nix so the bundled Docker image
# (nix/docker.nix) reproduces this shell 1:1.
{ pkgs, lib, spaTools ? [ ] }:
let
  devEnv = import ./dev-env.nix { inherit pkgs lib spaTools; };
  # `export NAME=value` lines for every shared env var, in deterministic order.
  exports = lib.concatStringsSep "\n"
    (lib.mapAttrsToList (n: v: "export ${n}=${lib.escapeShellArg v}") devEnv.env);
in
pkgs.mkShell {
  # buildTools + the Dioxus SPA toolchain (rust+wasm32, dx, wasm-bindgen-cli,
  # binaryen, lld) so `nix develop` can build launcher/spa-src via scripts/build-spa.sh.
  packages = devEnv.packages;
  shellHook = ''
    ${exports}

    # Submodules the build needs: plan-ai-design (SPA component lib) + mac-mgmt
    # (mac-mgmt-services, the launcher's control-plane dep). Init whichever is missing.
    if [ -f .gitmodules ]; then
      [ -e third_party/plan-ai-design/assets/input.css ] || \
        { echo "==> initialising plan-ai-design submodule"; git submodule update --init --recursive third_party/plan-ai-design || true; }
      # --recursive: mac-mgmt has its own nested submodules (memvault → design) that
      # nix's self.submodules fetch insists on, though the launcher only uses the
      # self-contained mac-mgmt-services crate.
      [ -e third_party/mac-mgmt/mac-mgmt-services/Cargo.toml ] || \
        { echo "==> initialising mac-mgmt submodule"; git submodule update --init --recursive third_party/mac-mgmt || true; }
    fi
    if [ -f usb.lock ]; then
      echo "plan-ai-usb-minimal — pinned versions:"
      jq -r '"  ollama     \(.ollama.version)\n  open-webui \(.openwebui.version)\n  python     \(.python)"' usb.lock
    fi
  '';
}
