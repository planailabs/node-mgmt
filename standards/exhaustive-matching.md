# Exhaustive-matching standard

When code branches over a **closed set of known variants** — a build target, a
platform/OS key, an ollama flavour, a state enum, a component format — every known
variant is handled explicitly and the leftover branch is an **explicit error**, never
a silent catch-all that picks one arm.

A catch-all `else`/`*)`/`_` that returns a *value* turns "I forgot a case" into a
wrong-but-plausible build: an arm64 launcher silently gets the x86_64 spinner, an
unknown target silently bundles the linux runtime. The failure surfaces far from the
omission — at runtime on the wrong hardware, not at the edit. Making the fallthrough
throw moves it back to the moment a new variant is added.

## 1. Map known cases explicitly, throw on the rest

The last branch is reserved for the genuinely-unhandled input and it *fails loudly*
with the offending value in the message. Do not let it double as a real case.

**Nix** (`flake.nix`, `launcherFor` spinner selection):

```nix
spinnerPkg =
  if      lib.hasInfix "windows" zigTarget then spinnerFor { ... }
  else if lib.hasInfix "apple-darwin" zigTarget then spinnerFor { ... }
  else if lib.hasInfix "aarch64" zigTarget then spinnerFor { zigTarget = "aarch64-unknown-linux-gnu"; ... }
  else if lib.hasInfix "x86_64"  zigTarget then spinnerFor { zigTarget = "x86_64-unknown-linux-gnu"; ... }
  else throw "launcherFor: no spinner mapping for zigTarget '${zigTarget}'";
```

The x86_64-linux case is spelled out; the final `else` is a `throw`, not "default to
x86_64". Adding `riscv64-unknown-linux-gnu` later fails the build with a clear message
instead of silently shipping an x86_64 spinner.

**Bash** (`scripts/lib.sh`, `scripts/make-runtime.sh`, `scripts/bundle.sh`): a
`case "$TARGET"` over targets ends in `*) die "unknown target: $1" ;;` — never a `*)`
that echoes a default triple/attr.

**Rust** (`xtask`): validate against the known set and `bail!` on anything else
(`resolve_targets` rejects a target not in `KNOWN_TARGETS`); `match` on the contract
enums (`ServiceState`, `UpdateState`) with no `_ =>` wildcard, so a new variant is a
compile error — see [control-api.md](control-api.md) §3.

## 2. One source of truth for the known set

The closed set lives in exactly one place (`KNOWN_TARGETS` / `KNOWN_OLLAMA` in
`xtask/src/main.rs`, the enum in `crates/control-api`). Branches map *from* it; they
do not re-enumerate it. Adding a variant there and forgetting a branch then trips the
throw/`die`/compile-error in §1 rather than slipping through.

## 3. Prefer compile-time exhaustiveness where the language gives it

A Rust `match` on an enum with no wildcard arm is checked by the compiler — strictly
better than a runtime throw. Reach for the runtime error (§1) only where the matched
value is an open type (a `&str` target key, a nix string) the compiler can't close.
