# plan.ai standards

Conventions that hold across the codebase. Each file is the source of truth for one
area — code is expected to match it, and changes to the convention land here first.

| Standard | Scope |
|---|---|
| [control-api.md](control-api.md) | The `/api/*` control-plane HTTP surface: the shared contract crate, response/error shapes, state enums, and how clients and backends consume it. |

## Why these exist

The control plane has three independent touchpoints — the real launcher, the dev
mock-server, and the wasm SPA — that all speak the same `/api/*`. Without a written
contract they drift (and they did: `info()` was untyped, so the mock grew a
`platforms` key the launcher never sent; command endpoints replied with a bare
`"ok"` the client tried to parse as JSON). A standard plus a single typed crate makes
that class of bug a compile error instead of a runtime surprise.
