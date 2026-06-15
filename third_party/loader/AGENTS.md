# AGENTS.md — loader-builder

Guidance for AI agents (and humans) working on **loader-builder** — the reusable,
**project-agnostic** framework for building and running a portable, mount-from-a-pool
application loader (the kind that ships components as squashfs/dmg/dir, mounts them
beside a native launcher, enters a NixOS FHS sandbox when needed, supervises services,
and updates itself in place).

This repo is consumed as a **git submodule** (`git@git.plan.ai:plan-ai/loader-builder`).
The first consumer is **plan-ai-usb-minimal**; the whole point of this repo is that a
*second* product can vendor it, write its own `loader.toml` + a thin launcher, and get
the same build pipeline + runtime substrate for free. **Keep it generic** — nothing
plan.ai-specific belongs here; all product specifics live in the consumer's
`loader.toml` and its launcher/usbd binaries.

---

## The one idea that shapes everything

**One declarative `loader.toml` (in the consumer repo) drives the entire build.** The
build tool (`xtask`) + the render engine (`loader-engine`) read it and emit a ninja
graph, the update manifest, and the `Makefile`. The nix library (`nix/loader`) packs
components and builds the FHS + the devshell. The runtime crate (`loader-core`) carries
the mount/FHS/splash/update substrate the consumer's launcher drives. **The framework
contains logic; the consumer's `loader.toml` contains data.**

```
consumer repo                          loader-builder (this submodule)
─────────────                          ───────────────────────────────
loader.toml  ───────────reads────────▶ crates/xtask  ──uses──▶ crates/loader-engine ──▶ build.ninja + Makefile
                                                      ──uses──▶ crates/loader-manifest ─▶ update.json (ClassifyTable)
flake.nix    ──imports──▶ nix/loader  { mkSqfs, mkDmg, mkClosureSqfs, mkExtractDir,
                                        mkNixosFhs(fhs.nix), readStores(stores.nix),
                                        mkDevShell, mkDevImage (devshell.nix) }
launcher/    ──deps─────▶ crates/loader-core  { net, FHS-entry, mount, pool, splash,
                                                cache+lock — re-exported, lifecycle-driven }
scripts (ninja leaves) ─▶ scripts/  { store-import, nix-component, import-build-component,
                                       pack-component, lib, bundle, make-usb-image }
```

---

## Layout

```
crates/
  loader-manifest/   update-manifest schema (Entry/Manifest/Selection) + sha256 + diff.
                     classify()/classify_feature() are ClassifyTable methods built from
                     loader.toml — the consumer's platform/feature layout, NOT hardcoded.
                     The launcher updater never classifies at runtime (it diffs
                     already-classified entries) → ClassifyTable is build-side only.
  loader-engine/     Pure render: parse loader.toml -> Config; render() emits the ninja
                     graph; render_makefile() emits the Makefile; resolve_targets()/
                     flavour_keys_for() do the target/flavour selection. No IO except the
                     injected SrcFns (so it's unit-testable). @loader/<script> tokens are
                     rewritten to the loader scripts dir at render time.
  xtask/             The build tool (`nix run .#xtask`). Subcommands: gen-ninja, build,
                     gen-makefile, gen-manifest, components-manifest, image-prep, tarball,
                     upload. Everything is loader.toml-driven; no consumer constants.
  loader-core/       The runtime substrate the consumer's launcher links + re-exports:
                     net (shared HTTP client), the FHS entry (is_nixos/maybe_run_in_fhs/
                     fhs_store_mode), component mount (provide/teardown/ensure_tool/…/
                     Mount), pool discovery (components_dir/resolve_pool/…), splash
                     (Splash/show_splash/…), cache_root + acquire_instance_lock. build.rs
                     embeds the static squashfuse_ll/unsquashfs/bwrap/spinner blobs.
nix/loader/
  packers.nix        mkSqfs / mkClosureSqfs / mkExtractDir / mkDmg (the last curried with
                     the consumer's libdmg-hfsplus). Flags match scripts/lib.sh so the
                     loader mounts nix-built and script-built components identically.
  fhs.nix            mkNixosFhs: buildFHSEnv builder; the package set is data (dotted
                     names resolved via getAttrFromPath) from loader.toml [fhs].
  stores.nix         The dist/.stores reader (consumer passes its dir).
  devshell.nix       mkDevShell (interactive `nix develop`) + mkDevImage (the same env as
                     a dockerTools image with nix inside). Parameterized by the consumer's
                     packages + env + project shellHook + extra packages/contents.
  default.nix        `import nix/loader { pkgs, nixpkgs }` → all of the above.
scripts/             The generic packing scripts (run as ninja leaves). They honour
                     PLANAI_REPO_ROOT so a vendored lib.sh keeps REPO_ROOT = the project
                     root (NOT the submodule) — load-bearing; see below.
```

---

## `loader.toml` schema (lives in the consumer repo)

The engine deserializes it into `loader_engine::Config`. Sections:

- **`[manifest]`** — `product`, `update_url`, `[manifest.classify]` (`exact`/`substring`/
  `tools` → platform classification), `[manifest.image_prep]` (default-off dropping +
  platforms.json seeding), `[manifest.upload]`. Feature tagging is **derived from each
  `[[component]].feature`** (the build step is authoritative — no basename guessing; see
  `Config::feature_classify_table`); `[manifest.feature_classify]` is optional and merged
  on top only to override or cover files no component produces.
- **`[layout]`** — launcher artifact names per target, image name, volume label, the
  drive-root dir names (consumed by `bundle.sh` / `make-usb-image.sh`).
- **`[targets]`** — `known`, `select_env` (the env knob, e.g. `PLANAI_PLATFORMS`),
  `select_lock` (a usb.lock array), `[targets.alias]` (e.g. nixos-x64→linux-x64),
  `[targets.os_match]` (OS-family substrings).
- **`[[feature]]`** — `name` + `default` (on/off).
- **`[fhs]`** — `name`, `run_script`, `systems`, `target_pkgs` (dotted nixpkgs names).
- **`[flavours.<name>]`** — `keys` (per-GPU expansion for ollama / llama.cpp / …).
- **`[[step]]`** — leaf build steps (download/wheel/app/spa/runtime/models): `cmd`,
  `deps`, `src`/`src_tree`, `desc`, optional `per = "target"`.
- **`[srcgroups]`** — named source-input bundles reused as deps (`@loader/x` → loader
  scripts dir; bare paths stay project-relative).
- **`[[component]]`** — the build graph's components. `per = shared|target|flavour|
  shared-os`, `group` (1 = the ow-assets/runtime/ollama pass, 2 = the rest), `builder =
  pack|nix|import-build`, a per-OS `[component.format]` map (`ext`/`attr`/`desc`/`out`/
  `stamp`, templated with `{target}`/`{key}`), `src_stamp`, `srcgroup`, `src_extra`,
  `src_tree`. The engine reproduces the historical hand-written graph from this.
- **`[bundle]` / `[[artifact]]`** — the per-target bundle + the final image/tarball edges.
- **`[make]` + `[[make.target]]`** — the Makefile: `xtask_cmd`, `devshell_var`,
  `guard_exclude`, `default_goal`, and one entry per target (`name`/`help`/`recipe`
  lines/`deps`/`phony`). `render_makefile` emits a generic preamble + these.

---

## How a NEW product consumes this

1. Add the submodule: `git submodule add git@git.plan.ai:plan-ai/loader-builder third_party/loader`.
2. Write `loader.toml` (copy plan-ai-usb-minimal's and edit the data).
3. `flake.nix`:
   - `nix run .#xtask` built from `third_party/loader/crates/{xtask,loader-engine,loader-manifest}` (`buildRustPackage`, `cargoLock.lockFile = …/xtask/Cargo.lock`).
   - `loader = import ./third_party/loader/nix/loader { inherit pkgs; nixpkgs = …; }`,
     then `loader.mkSqfs`/`mkDmg`/`mkClosureSqfs`/`mkExtractDir` in your `nix/builds.nix`,
     `loader.mkNixosFhs { system; targetPkgNames = loaderToml.fhs.target_pkgs; }`,
     `loader.readStores ../dist/.stores`, and `loader.mkDevShell`/`mkDevImage`.
   - Copy the submodule `loader-manifest` + `loader-core` into the launcher build tree
     (they're path deps; `self.submodules = true` brings the source into the flake).
4. The launcher: `loader-core = { path = "../third_party/loader/crates/loader-core" }`,
   `pub(crate) use loader_core::*;` for the substrate, and drive it from your lifecycle.
5. `make` (the Makefile is generated by `xtask gen-makefile`, committed so it can trigger xtask).

---

## Load-bearing details (expensive to re-discover)

- **`PLANAI_REPO_ROOT`.** `scripts/lib.sh` sets `REPO_ROOT="${PLANAI_REPO_ROOT:-$SCRIPT_DIR/..}"`.
  When vendored at `third_party/loader/scripts`, `$SCRIPT_DIR/..` would resolve to the
  submodule and re-root `dist/`, the `nix build` cwd, and `dist/.stores` into it. The
  build engine (`xtask build`) exports `PLANAI_REPO_ROOT=<project root>` before ninja, so
  the vendored scripts keep the project root. Never remove this.
- **`@loader/` tokens.** In `loader.toml` cmds/srcs, `@loader/x` is rewritten at render
  time: in a cmd → `./<loader_dir>/x` (executable), in a dep → `<loader_dir>/x` (no `./`).
  `loader_dir` defaults to `third_party/loader/scripts`; `PLANAI_LOADER_DIR=scripts` (the
  in-tree value) lets a test render the pre-cutover graph.
- **nix consumes the lib by relative `import`, not a flake input.** The consumer's
  `nix/builds.nix` is evaluated `--impure` (it reads `dist/.stores`, uses
  `builtins.storePath`); a flake input would force purity + a separate lock. `import
  ../third_party/loader/nix/loader { … }` + `self.submodules = true` is the pattern.
- **The blobs feed `loader-core`'s build.rs.** The flake sets `PLANAI_SQUASHFUSE_LL` /
  `PLANAI_UNSQUASHFS` / `PLANAI_BWRAP_BIN` / `PLANAI_SPINNER_BIN`; cargo passes env to ALL
  build scripts, so `loader-core`'s build.rs (not the launcher's) embeds them. The
  consumer's launcher build.rs only wires its own assets (e.g. the SPA).
- **`ClassifyTable` is build-side only.** Don't reach for `classify_feature` at runtime in
  a launcher — the updater diffs already-classified `Entry`s.

---

## Build & test (from a consumer checkout, inside `nix develop`)

```
cargo test -p loader-manifest -p loader-engine   # schema/diff + graph soundness
cargo build -p loader-core -p xtask              # substrate + tool (loader-core needs the blob env on linux)
nix build .#xtask .#nixosFhs .#launcher-<t>      # the cut-over build path
nix run .#xtask -- gen-ninja                     # the real graph (PLANAI_PLATFORMS=… to subset)
nix run .#xtask -- gen-makefile                  # regenerate the committed Makefile
make image PLATFORMS=linux-x64                   # end-to-end (downloads + nix builds)
```

`loader-engine`'s `renders_sound_graph` test validates the emitted graph structurally
(rules + `default`, expected edges, no duplicate outputs, every stamp dep has a
producer) without depending on a freshly-regenerated build.ninja.

---

## Conventions

- **Generic only.** A change that hardcodes a product name, a specific component, a fixed
  package list, or a plan.ai path does NOT belong here — push it to `loader.toml`.
- **Match the consumer's idioms** when extracting more from a launcher: keep behaviour
  byte-identical, parameterize the data, and verify with `nix build` + the consumer's
  tests before committing.
- **Keep the render pure.** `loader_engine::render`/`render_makefile` must stay IO-free
  (filesystem reads only via the injected `SrcFns`) so they remain unit-testable.
