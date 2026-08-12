# ADR-0001: Dynamic plugin registry for connectors

Status: Proposed
Date: 2026-08-12

## Context

`baileyrd/Daily-Backup-System` discovers connectors through Python's
`importlib.metadata` entry points (`dbs.connectors` group): built-in and
third-party connectors load through the same mechanism, one code path, no
built-in/plugin drift (`src/dbs/core/registry.py`). Robustness is the
point of that module — a connector that fails to import, isn't a
`Connector` subclass, declares a malformed `type`, or targets an
incompatible `core_api_version` must never crash discovery of the others.
Collision resolution has explicit precedence: config override wins
outright; otherwise a built-in is shadow-protected unless
`allow_override` is set; otherwise a deterministic sort among third
parties picks a winner. Every load failure and every shadowed connector
is recorded and surfaced (`dbs connectors list --verbose`).

`rusty_dbs`'s round-1 scope decision (`gap-analysis.md`, confirmed with
the repo owner) is **true dynamic plugin loading** — connectors are
separate artifacts loaded at runtime, not compiled into one binary —
closer to Python's decoupled-package model than a static registry. That
decision named two candidate shapes: "`cdylib` + a stable ABI, or a
subprocess/IPC boundary." This ADR picks between them.

Rust has no stable ABI. The default calling convention and type layout can
change between compiler versions, and even between two builds of the
*same* compiler version with different codegen flags. A `cdylib` exposing
the `Connector` trait as defined in `crates/dbs-core/src/connector.rs`
(#4) — `String`, `Vec<T>`, `Box<dyn Iterator<...>>`, `chrono::DateTime`,
`serde_json::Value` throughout — cannot cross a dynamic-library boundary
safely without either (a) locking host and every plugin to the exact same
rustc version and dependency versions, or (b) a real FFI-safe abstraction
layer (e.g. the `abi_stable` or `stabby` crates) translating every type at
the boundary.

## Decision

**Subprocess + line-delimited JSON IPC**, not a `cdylib`.

Each connector is a separate compiled executable (`dbs-connector-<type>`,
naming convention). The host spawns it as a child process and speaks a
small JSON-RPC-shaped protocol over its stdin/stdout:

1. **Handshake.** On startup the connector writes one JSON line
   self-describing its contract: `type`, `core_api_version` (now a
   *protocol* version, negotiated data instead of an ABI compatibility
   problem), `schema_version`, `capabilities`, `secret_keys`, `item_kinds`,
   `display_name`/`description`/etc. — the same fields `_validate_contract`
   checks in the reference, just validated against JSON instead of a
   Python class's attributes.
2. **Run.** The host writes one JSON line carrying a serializable
   `RunContext` (cursor, since-watermark, mode, limit, resolved secret
   values for the keys the connector declared — the subprocess boundary
   *is* the least-privilege enforcement point, arguably more airtight than
   the reference's in-process `Secrets` accessor).
3. **Stream.** The connector writes one JSON line per `FetchEvent`
   (`Item`/`Checkpoint`/`ReconcileMarker`) to stdout, and a final
   line reporting terminal status (`ok` or a `ConnectorError` variant +
   message). The host reads and processes lines as they arrive — no
   buffering the whole stream, matching the reference's per-checkpoint
   commit behavior.
4. **Registry.** A manifest (`connectors.toml` or a directory scan of
   `dbs-connector-*` on `PATH`/a configured connectors directory) replaces
   entry-point metadata as the discovery mechanism. Loading a manifest
   entry means spawning the process and reading its handshake — a
   connector that fails to start, hangs on handshake past a timeout, or
   sends a malformed handshake is recorded as a `LoadFailure` exactly like
   the reference, and never crashes discovery of the others.
5. **Collision resolution** (explicit override > built-in shadow
   protection > deterministic third-party sort) ports directly — it
   operates on the manifest/handshake data, not on Rust types, so nothing
   about moving to IPC changes that logic.

## Alternatives considered

**`cdylib` + `abi_stable`** (or `stabby`). Keeps the ergonomic
trait-object design already built in #4 (`Box<dyn Connector>`) working
essentially unchanged across the boundary, and avoids per-item
serialization overhead. Rejected for round 1: `abi_stable` requires
rewriting every type crossing the boundary in its `#[repr(C)]`-friendly
vocabulary (`RString`, `RVec`, `RBox<dyn Trait>`, ...), the host and every
plugin must be rebuilt together on a dependency-version bump (a much
tighter lockstep requirement than the subprocess approach's protocol
versioning), and a bug at the FFI boundary is undefined behavior, not a
recoverable error — a strictly worse failure mode than a subprocess that
crashes and gets recorded as a `LoadFailure`. Worth revisiting only if
per-item IPC overhead becomes a measured bottleneck, which is unlikely
for a personal-backup-tool's item throughput.

**Hand-rolled C-ABI (`extern "C"`, `#[repr(C)]`, no `abi_stable`).**
Same lockstep/UB problems as above, with none of `abi_stable`'s tooling
to catch mistakes — strictly worse than either alternative.

**Compiled-in static registry.** Simplest, safest, no IPC/FFI risk at
all — but explicitly rejected by the round-1 scope decision in favor of
true dynamic loading, so it's not on the table for this ADR; noted here
only because it's `gap-analysis.md`'s documented fallback if this
decision needs to be revisited later.

## Consequences

- **New dependency, deferred to the implementation issue.** This ADR
  doesn't itself add a crate — `libloading` isn't needed (no `dlopen`,
  just `std::process::Command`), but the implementation issue will likely
  want a small JSON-line-framing helper. Confirm at that point whether
  `serde_json`'s existing line-delimited support is sufficient or a
  dedicated crate is worth it — either way, that's its own stop-and-ask
  per this skill's dependency rule, not pre-approved by this ADR.
- **Per-connector process overhead.** Spawning a process per source per
  run is heavier than an in-process call. Acceptable for a backup tool
  that runs sources sequentially-by-default and `--parallel N` at most a
  handful at a time (see `gap-analysis.md`'s `dbs backup --parallel N`
  row) — revisit if profiling says otherwise.
- **Least-privilege secrets get stronger, not weaker.** A subprocess
  literally cannot read a secret it wasn't handed on its stdin — an
  even harder boundary than the reference's `Secrets` accessor object,
  which is an in-process convention a bug could bypass.
- **Windows/Linux parity is simpler than the `cdylib` path would have
  been.** `std::process::Command` behaves the same on both; there's no
  `.so`-vs-`.dll` loading difference to special-case, which directly
  serves the round-1 cross-platform floor decision.
- **Every connector issue filed against `gap-analysis.md`'s Connectors
  section now implies "a `dbs-connector-<type>` binary speaking this
  protocol," not "a Rust module implementing `Connector` directly.**"
  The in-process `Connector` trait from #4 still matters — it's the shape
  a connector's *internal* implementation follows before being wrapped by
  the subprocess handshake/protocol shim this ADR describes; that shim is
  the next issue once this ADR is accepted.
- **Follow-up issue:** implement the handshake/protocol shim + manifest-based
  registry described in "Decision" above, once this ADR is reviewed and
  accepted (not bundled into this ADR issue, which is docs-only).
