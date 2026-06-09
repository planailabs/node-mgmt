# Reads the per-import records written by scripts/store-import.sh
# (dist/.stores/<name> contains a single store path) into an attrset
#   { <name> = builtins.storePath "/nix/store/…"; }
#
# PER-STEP by construction: each store-import writes its OWN record, and this reads
# whatever is present at eval time — there is no whole-graph "gen-stores" barrier to
# re-run. A new/changed import is picked up on the next `nix-build --impure` with no
# regeneration step. Requires --impure (storePath + reading dist/.stores outside the
# flake). Empty until the first import is recorded.
let
  dir = ../dist/.stores;
  read = name: builtins.storePath (builtins.readFile (dir + "/${name}"));
in
if builtins.pathExists dir
then builtins.mapAttrs (name: _type: read name) (builtins.readDir dir)
else { }
