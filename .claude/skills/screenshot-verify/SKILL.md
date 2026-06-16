---
name: screenshot-verify
description: Visually verify plan.ai SPA (Dioxus dashboard) UI changes by screenshotting the live app against the mock backend. Use after editing launcher/spa-src (tabs, app switcher, dashboard, config views) to confirm the rendered result — opens menus/dropdowns and captures PNGs via headless Chrome. Triggers on "screenshot", "verify the UI", "show me how it looks", "check the dashboard renders".
---

# Screenshot-verify the plan.ai SPA

Render the Dioxus SPA (`launcher/spa-src`) against the mock control API and
capture a screenshot — including interactive states (open the app switcher, a
dropdown, a tab) — so UI changes can be checked, not just typechecked.

All tooling (`dx`, `tailwindcss`, `google-chrome`, `node`) is in the Nix
devshell. No npm install — `screenshot.mjs` uses Node's global `WebSocket`/`fetch`.

## Steps

1. **Build the SPA and check it first.** A screenshot only reflects compiled
   code:
   ```
   cd launcher/spa-src && mkdir -p assets && touch assets/tailwind.css
   cargo check --target wasm32-unknown-unknown   # the SPA is wasm
   ```
   (`tailwind.css` must exist for the `asset!()` macro; `preview.sh` regenerates
   the real one.)

2. **Start the preview** (mock API + dx serve, in the background):
   ```
   .claude/skills/screenshot-verify/preview.sh
   ```
   It prints `PREVIEW_URL=http://127.0.0.1:<port>` once the wasm build finishes.
   Use that URL — **don't assume 8080**; dx falls back to another port if 8080
   is taken (e.g. a real Open-WebUI is running there).

3. **(Optional) toggle mock features** so optional UI appears. The mock starts
   with only `openwebui`; enable hermes/mgmt to see those apps:
   ```
   curl -s -X POST http://127.0.0.1:9999/api/platforms \
     -H 'content-type: application/json' \
     -d '{"platforms":["linux-x64"],"features":["openwebui","hermes","mgmt"]}'
   ```

4. **Screenshot** (the script launches its own fresh-profile Chrome and tears it
   down):
   ```
   node .claude/skills/screenshot-verify/screenshot.mjs \
     --url http://127.0.0.1:<port> --out /tmp/shot.png \
     --ready "header.topbar" \
     --click "() => { const b=[...document.querySelectorAll('button')].find(x=>x.textContent.includes('Apps')); b?.click(); return !!b; }"
   ```
   - `--ready` polls for a selector before shooting (use `header.topbar` — it
     means the wasm has hydrated).
   - `--click` runs a JS arrow-fn after hydration to open a menu/tab. Omit it for
     the default view.
   Then **Read** `/tmp/shot.png` to see the result.

5. **Tear down** when done: `.claude/skills/screenshot-verify/stop.sh`

## Gotchas (learned the hard way)

- **Service-worker hijack.** Open-WebUI is a PWA; once Chrome visits it on a
  port, its service worker serves a cached app shell on that origin even after
  dx takes the port. `screenshot.mjs` sidesteps this by always using a fresh
  throwaway `--user-data-dir`. If you drive Chrome yourself, do the same.
- **dx port fallback.** `dx serve` wants 8080 but silently moves if it's taken.
  Always read `PREVIEW_URL` from `preview.sh` (it greps the `dx-wrapped`
  listener) rather than hardcoding.
- **Hot-reload is unreliable** (the bundled `dx` 0.7.x vs dioxus 0.8-alpha emit
  a version-mismatch warning). After editing source, restart dx — re-run
  `stop.sh` then `preview.sh` — instead of trusting a live rebuild.
- **Mock state is in-memory.** Feature toggles (step 3) reset when the mock
  restarts. Service readiness random-walks, so an app may show "starting" one
  shot and "ready" the next — that's the mock, not a bug.
