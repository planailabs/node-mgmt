# loader-builder

Project-agnostic loader framework, vendored as a git submodule
(`git@git.plan.ai:plan-ai/loader-builder`). A consuming product writes one
`loader.toml` + an `impl loader_core::Project` and gets the same FHS-entry,
lifecycle, mount, update, and build pipeline.

- `crates/loader-manifest` — update-manifest schema + diff; classification is
  data-driven via `ClassifyTable` (built from the consumer's `loader.toml`).
- `crates/loader-engine` — parses `loader.toml` and renders the ninja build graph.
- `crates/xtask` — the build tool (all subcommands generic; `nix run .#xtask`).
- `crates/loader-core` — runtime: lifecycle state machine + FHS entry + mount +
  update/apply, behind a `Project` trait. (WIP)
- `nix/loader` — packers (mkSqfs/mkDmg/…), the FHS builder, store-import glue.
- `scripts` — the generic packing scripts (store-import, nix-component,
  import-build-component, pack-component, bundle, make-usb-image, lib).

See the consumer's `loader.toml` for the full schema.
