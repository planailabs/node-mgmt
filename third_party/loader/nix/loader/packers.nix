# Generic component packers, extracted verbatim from the consumer's nix/builds.nix.
# The flags match scripts/lib.sh (pack_squashfs / pack_dmg) so the loader mounts the
# nix-built artifacts identically to the script-built ones.
{ pkgs }:
let lib = pkgs.lib; in
rec {
  # squashfs of a source dir (zstd, all-root, no-xattrs) — content-addressed cache.
  mkSqfs = name: src: pkgs.runCommand "${name}.squashfs"
    { nativeBuildInputs = [ pkgs.squashfsTools ]; }
    "mksquashfs ${src} $out -comp zstd -processors $NIX_BUILD_CORES -all-root -no-xattrs -noappend -quiet";

  # A closure's store paths packed as a squashfs whose ROOT holds each path by its
  # hash-name, so <mnt>/<hash> == /nix/store/<hash>. The loader squashfuse-mounts this
  # and provides it as /nix/store inside an outer namespace (no nix-store --import).
  mkClosureSqfs = ci: pkgs.runCommand "nixos-fhs.squashfs"
    { nativeBuildInputs = [ pkgs.squashfsTools ]; }
    "mksquashfs $(cat ${ci}/store-paths) $out -comp zstd -processors $NIX_BUILD_CORES -all-root -no-xattrs -noappend -quiet";

  # Extract a tarball to a plain directory (used in place on FAT32 / feeds mkDmg).
  mkExtractDir = name: tarball: pkgs.runCommand name { nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ]; }
    "mkdir -p $out && tar -xf ${tarball} -C $out";

  # mac dmg, built in a Linux VM (privileged loop-mount of HFS+, then UDIF via libdmg).
  # `libdmg` is the consumer's libdmg-hfsplus derivation (passed in — project pin).
  mkDmg = { libdmg }: { name, src, vol ? "PlanAI", memSize ? 2048 }:
    let
      vmTools = pkgs.vmTools.override {
        rootModules = [ "virtio_pci" "virtio_mmio" "virtio_blk" "virtio_balloon"
          "virtio_rng" "ext4" "virtiofs" "crc32c" "loop" "hfsplus" ];
      };
    in
    vmTools.runInLinuxVM (pkgs.runCommand "${name}.dmg"
      { nativeBuildInputs = [ pkgs.hfsprogs pkgs.util-linux ]; inherit memSize; }
      ''
        raw=$TMPDIR/raw.hfs
        kb=$(du -slk ${src} | cut -f1); nfiles=$(find ${src} | wc -l)
        truncate -s $(( kb * 1024 + nfiles * 4096 + 256*1024*1024 )) $raw
        mkfs.hfsplus -v "${vol}" $raw
        mkdir -p $TMPDIR/mnt
        mount -t hfsplus -o loop $raw $TMPDIR/mnt
        cp -a --no-preserve=links ${src}/. $TMPDIR/mnt/
        umount $TMPDIR/mnt
        ${libdmg}/bin/dmg dmg $raw $out
      '');
}
