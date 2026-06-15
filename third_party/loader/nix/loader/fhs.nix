# The NixOS FHS helper builder, parameterized so the consumer supplies the package
# set from loader.toml `[fhs].target_pkgs` (dotted names like `gcc-unwrapped.lib`
# resolved via getAttrFromPath). Built per linux arch (the closure is the target
# machine's /nix/store paths). See the consumer's flake for the call site.
{ nixpkgs }:
{ system, name ? "planai-fhs", runScript ? ''exec "$@"'', targetPkgNames }:
let
  fhsPkgs = import nixpkgs { inherit system; };
  lib = fhsPkgs.lib;
  resolve = n: lib.getAttrFromPath (lib.splitString "." n) fhsPkgs;
in
fhsPkgs.buildFHSEnv {
  inherit name;
  runScript = "${fhsPkgs.writeShellScript "${name}-run" runScript}";
  targetPkgs = _p: map resolve targetPkgNames;
}
