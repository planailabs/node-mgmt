# Layer 3: the Open-WebUI python runtime via uv2nix. uv2nix turns
# runtime/uv.lock into wheel FODs; mkVirtualEnv builds a venv (store-bound).
# Relocation onto the portable pbs interpreter happens in scripts/make-runtime.sh
# (a vanilla copy of the site-packages) so the shipped result runs outside the store.
{ pkgs, lib, pyproject-nix, uv2nix, pyproject-build-systems, workspaceRoot }:
let
  workspace = uv2nix.lib.workspace.loadWorkspace { inherit workspaceRoot; };
  overlay = workspace.mkPyprojectOverlay { sourcePreference = "wheel"; };
  python = pkgs.python312;
  pythonSet = (pkgs.callPackage pyproject-nix.build.packages { inherit python; })
    .overrideScope (lib.composeManyExtensions [
      pyproject-build-systems.overlays.default
      overlay
    ]);
  runtimeVenv = pythonSet.mkVirtualEnv "plan-ai-runtime" workspace.deps.default;
in
{ inherit runtimeVenv; }
