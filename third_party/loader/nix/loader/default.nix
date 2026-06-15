# The loader nix library. A consumer imports this with its pinned pkgs + nixpkgs:
#
#   loader = import ../third_party/loader/nix/loader { inherit pkgs; nixpkgs = flake.inputs.nixpkgs; };
#   loader.mkSqfs "ow-assets" (loader.readStores ../dist/.stores).ow-assets;
#   loader.mkNixosFhs { system = "x86_64-linux"; targetPkgNames = fhsPkgs; };
#   (loader.mkDmg { inherit libdmg; }) { name = "ow-assets"; src = …; };
{ pkgs, nixpkgs }:
let
  packers = import ./packers.nix { inherit pkgs; };
  fhs = import ./fhs.nix { inherit nixpkgs; };
in
packers // {
  # buildFHSEnv builder; targetPkgNames from loader.toml [fhs].target_pkgs.
  mkNixosFhs = fhs;
  # dist/.stores reader (pass the consumer's dir).
  readStores = distStores: import ./stores.nix { inherit distStores; };
}
