# Control-plane API standard

The SPA talks to its backend over a same-origin `/api/*` HTTP surface. There are two
backends — the real launcher (`launcher/src/serve.rs`) and the dev mock
(`mock-server/src/main.rs`) — and one client (the wasm SPA, `launcher/spa-src`). All
three are kept in lock-step by a single crate. These rules keep them honest.

## 1. One typed contract crate

`crates/control-api` (`plan-ai-control-api`) owns the entire contract:

- **`types`** — every request/response DTO and state enum, serde-only. No axum, no
  tokio. The wasm SPA depends on this crate with `default-features = false` and
  deserializes responses into these exact structs.
- **`server`** (feature `server`, on by default) — the `ControlApi` trait, the
  `router`, and the axum handlers. The launcher and mock each `impl ControlApi`;
  neither defines its own routes or JSON shapes.

A new endpoint is added once, here, as a trait method + a typed DTO. It is then a
compile error for any backend not to provide it, and the SPA gets the type for free.

## 2. No `serde_json::Value` on the contract

Every endpoint has a named DTO. `Value` is banned from request/response signatures —
it is exactly what let `info()` drift. The **one exception** is the llmfit model
browser (`/api/llmfit/*`): it is an opaque reverse-proxy to an upstream service we do
not own, so it passes bytes through (`ProxyReply`) and the SPA reads those responses
as `Value`. Anything that is *our* contract is typed.

The **second exception** is the config plane (`/api/config`, `/api/config/schema`):
the config is a **schema-driven** document edited by the shared
`mac_mgmt_config_ui::ConfigEditor`. Its shape is the daemon's `UsbConfig` JSON Schema
(served at `/config/schema`), not a Rust DTO the SPA knows at compile time, so both the
config body and the schema are opaque `Value`. The launcher reverse-proxies these to
the USB daemon's loopback control server (which owns validation + persistence); the
mock serves the real `UsbConfig` schema + a sample.

## 3. State is an enum, declared once

`ServiceState` (`ready` / `starting` / `stopped` / `error`) and `UpdateState`
(`idle` / `checking` / `downloading` / `ready` / `applying` / `failed`) are Rust
enums with `#[serde(rename_all = "snake_case")]`. The wire strings exist in exactly
one place. UI colours and labels `match` on the enum; they never re-spell the
strings, so a renamed state can't silently desync a `match` arm.

## 4. Response shapes

| Kind | Method | Success | Body |
|---|---|---|---|
| Read | `GET` | `200 OK` | typed JSON object |
| Stream | `GET` | `200 OK` | `text/event-stream` (SSE) |
| Command | `POST` | `204 No Content` | **empty** |
| (any) error | — | `4xx` / `5xx` | `{"error": "<message>"}`, `application/json` |

Commands are state changes with nothing to return (`/api/ready`,
`/api/services/{id}/{action}`, `/api/update/check`, `/api/update/apply`,
`/api/platforms` POST). They reply `204` with an empty body — **never** a bare-word
`"ok"`, which is not valid JSON and which a JSON client cannot parse.

Errors are always the `ApiError` JSON object with a correct `content-type`. A
plain-text body served as `application/json` (as the proxy 502/503 path once did) is
a bug.

## 5. JSON field naming

`snake_case` for every field (`webui_url`, `ollama_port`, `progress_pct`). Enforced
by serde defaults — do not hand-write camelCase.

## 6. Routes

`/api/<area>[/<resource>][/<action>]`, lowercase, collections plural. Path
parameters are extracted, not string-sliced. The known set of an enumerable path
parameter (e.g. the service action `start|stop|restart`) is validated in the shared
handler, which returns `400 {"error": ...}` for an unknown value — so no backend has
to re-implement that check.

Config plane: `GET /api/config` (stored config), `GET /api/config/schema`
(`UsbConfig` JSON Schema), `PUT /api/config` (validate → persist → live-apply;
`200` with the daemon's `{ok, applied, …}` reply, or `422 {"error": ...}`). The real
launcher proxies these to the daemon (`PLANAI_USBD_URL`).

## 7. Service identity

The contract's service ids are canonical: `ollama`, `webui`. If a backend's
supervisor uses a different internal name (the launcher's is `open-webui`), the
mapping lives behind that backend and never reaches the client. One helper does the
translation; the alias is not sprinkled through call sites.

## 8. Client discipline

The SPA's `api` module exposes exactly:

- `get::<T>(path)` — typed read, deserializes into a contract DTO.
- `command(path)` / `command_json(path, body)` — fire a command, succeed on any
  `2xx` (incl. `204`) **without reading the body**, surface `ApiError` on failure.
- the SSE subscription for `/api/logs`.
- `get_json` / `post_json` returning `Value` — **only** for the `/api/llmfit/*`
  proxy (rule 2's exception).

No component poke at `Value` for an endpoint that has a DTO, and no command goes
through a JSON-parsing helper.

## Checklist for a new endpoint

1. Add the DTO(s) and any new enum to `types`.
2. Add the trait method to `ControlApi` and a handler + route to `router`.
3. Implement it in **both** `serve.rs` and the mock.
4. Consume it in the SPA via `get::<T>` / `command`.
5. If it's a command, it returns `204`; if it can fail, it returns `ApiError`.
