# Reads the per-import records written by scripts/store-import.sh
# (<distStores>/<name> contains a single store path) into an attrset
#   { <name> = builtins.storePath "/nix/store/…"; }
# distStores is the consumer's dist/.stores dir (passed in). Requires --impure.
{ distStores }:
let
  read = name: builtins.storePath (builtins.readFile (distStores + "/${name}"));
in
if builtins.pathExists distStores
then builtins.mapAttrs (name: _type: read name) (builtins.readDir distStores)
else { }
