# Archived: from-source Open-WebUI wheel build

A nix path that builds the `open_webui` wheel **from source** (frontend + wheel),
offline, via the store-import mechanism. **Not wired into the shipped build.**

## Why it's archived, not shipped

The shipped runtime installs `open_webui` from a **pinned PyPI wheel** (a fixed-output
derivation in `runtime/wheels-<target>.lock.json`) — already hermetic and pinned. This
from-source path reproduces the same wheel (verified: byte size within ~119 of the PyPI
wheel — zip metadata only), so it's an *alternative*, not a fix. We keep it for the day
we need to **patch open-webui** or **drop the PyPI dependency**.

## What's here

- `builds-from-source.nix` — the `ow-frontend` (vite) + `wheel` (hatchling) derivations.
- `fetch-ow-frontend.sh` — the impure boundary: `npm ci` + `pyodide:fetch`, then
  `store-import.sh` imports `ow-src` / `ow-node-modules` / `ow-pyodide`.

## Reviving it

1. Restore `fetch-ow-frontend.sh` to `scripts/`.
2. Fold `builds-from-source.nix`'s `let` bindings into `nix/builds.nix` (pass `pkgs` +
   `stores`), and `inherit ow-frontend wheel;` in the output set.
3. Re-add the ninja edges in `xtask render_ninja`: `ow-inputs` (runs the fetch script)
   → `ow-frontend` → `wheel`, all `nix-build --impure nix/builds.nix -A …`.
4. Point `nix/runtime.nix` at the local wheel store path instead of the PyPI FOD for
   `open_webui` (special-case it: it's a local store path, not a URL+hash FOD). Expect
   1–2 iterations here — `runtime.nix` assumes every wheel is a URL FOD.
5. Set `APP_BUILD_HASH` in the `ow-frontend` build if you want the in-app build hash to
   match a release (the custom hatch hook set it; we strip that hook).

The store-import foundation it relies on (`scripts/store-import.sh`, `nix/stores.nix`,
`nix/builds.nix`) is still live in the tree — used by the squashfs components + ow-assets.
