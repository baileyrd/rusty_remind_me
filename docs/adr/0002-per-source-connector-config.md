# ADR-0002: Per-source connector config over the subprocess wire

Status: Proposed
Date: 2026-08-13

## Context

Issue #164 wired up the last 13 `dbs-connector-*` crates as real ADR-0001
subprocess binaries. Doing so surfaced a gap (issue #166): every
connector's `main.rs` constructs its connector with `Config::default()` —
there is no channel from a source's real `[sources.NAME]` block into the
spawned process. `WireRunContext` (`crates/dbs-core/src/run_stream.rs`)
carries `secrets`, `cursor`, `since`, `mode`, and run-shape flags, but no
arbitrary per-source config.

For most connectors this is invisible: every `Config` field defaults to
something workable (page sizes, feature toggles), and auth is purely
secret-driven, same shape as `raindrop` (#161). Two of the 13 connectors
from #164 hit it directly, though: `dbs-connector-mastodon`'s
`MastodonConfig::instance` defaults to `""`, and `fetch()` hard-rejects
anything that doesn't look like a real `http(s)://` URL before ever
reaching the network — a real run fails every time, regardless of
secrets, because nothing tells it which instance to talk to.
`dbs-connector-podcast`'s `PodcastConfig::feeds` defaults to `[]` — this
connector has no separate "host" concept at all, the feed list *is* the
entire input — so a real run finds nothing, silently, every time.
`dbs-connector-bluesky`'s `identifier` has the same shape but happens not
to block a run today (nothing validates it before the HTTP call).

**The data already exists — it just doesn't cross the wire.**
`crates/dbs-core/src/config.rs`'s `SourceConfig` already has:

```rust
/// Connector-specific options: every key not in [`RESERVED_SOURCE_KEYS`].
pub options: HashMap<String, Value>,
```

populated by parsing every key of a `[sources.NAME]` TOML block that
isn't one of the nine reserved names (`type`, `enabled`, `schedule`,
`store_media`, `export`, etc.). So `instance = "https://example.social"`
under `[sources.my-mastodon]` already lands in
`config.sources["my-mastodon"].options["instance"]` today, with zero
config-parsing work needed. `service.rs::backup_source` already reads it:

```rust
let config_json = serde_json::to_string(&sc.options)...;
let source = self.storage.upsert_source(name, &sc.type_, &rc.plugin_id, &config_json, ...)?;
```

`SubprocessRunner::run_connector` (`run_stream.rs`, the production
`ConnectorRunner`) already holds `self.config` and already does
`let sc = self.config.sources.get(source_name);` to read
`store_media`/`max_media_mb` into `WireRunContext` — it just never reads
`sc.options`. The gap is exactly one hop wide: from a value the host
already has in hand to a field the wire struct doesn't carry.

## Decision

**Add `config: HashMap<String, serde_json::Value>` to `WireRunContext`,
carrying `sc.options` verbatim, plus a new default-no-op
`Connector::configure` trait method that each connector overrides only
if it needs per-source values.**

1. **`WireRunContext` gains one field:**
   ```rust
   /// This source's `options` map (every non-reserved `[sources.NAME]`
   /// TOML key) — the same map already serialized into `config_json`
   /// for `Storage::upsert_source`, now also reaching the connector
   /// that will actually use it. `#[serde(default)]` so a wire line
   /// from an older connector binary that doesn't know this field
   /// exists still deserializes.
   #[serde(default)]
   pub config: HashMap<String, serde_json::Value>,
   ```
   `SubprocessRunner::run_connector` populates it with `sc.options.clone()`
   — the exact map already flowing into `config_json` above, no new
   parsing/validation step at the host boundary.

2. **`Connector` gains one default trait method**
   (`crates/dbs-core/src/connector.rs`), matching the shape of the
   existing `open`/`close` default methods:
   ```rust
   /// Applies this source's `[sources.NAME]` config (every non-reserved
   /// key) to the connector's own fields, before `open`/`fetch`. Default
   /// no-op — most connectors need nothing beyond secrets and their own
   /// `Config::default()`. Called once, after the wire context arrives,
   /// never during discovery (a discovery-only spawn never sends one).
   fn configure(&mut self, _options: &std::collections::HashMap<String, serde_json::Value>) -> Result<(), ConnectorError> {
       Ok(())
   }
   ```
   A connector that needs a value — `mastodon` reading `instance`,
   `podcast` reading `feeds` — implements `configure()` to pull specific
   keys out of the map by name (the same name a user writes in
   `dbs.toml`), type-check/coerce them, and set the matching `Config`
   field(s) on `self`. A required-but-missing or malformed key returns
   `ConnectorError::Config(...)` — the same variant `open()`/`fetch()`
   already use, so it propagates over the wire via the existing
   `WireOutcome::Error`/`WireErrorKind::Config` path with no protocol
   change.

3. **`dbs_connector_support::run_connector_main`** (`subprocess_main.rs`)
   calls `connector.configure(&wire_ctx.config)` right after
   `read_wire_context()` succeeds, before `build_run_context`/
   `run_and_stream` — mirroring how a failing `open()` already short-
   circuits into `WireLine::Done(error_outcome(&e))` today:
   ```rust
   let Some(wire_ctx) = read_wire_context() else { return; };
   if let Err(e) = connector.configure(&wire_ctx.config) {
       write_line(&WireLine::Done(error_outcome(&e)));
       return;
   }
   let ctx = build_run_context(connector, wire_ctx);
   run_and_stream(connector, &ctx);
   ```

4. **No `main.rs` changes required for any connector.** The handshake
   (step 1) is written from a connector already constructed with
   `Config::default()` — it has to be, since a discovery-only spawn
   never sends a `WireRunContext` at all, and handshake output
   (`secret_keys`, `item_kinds`, `capabilities`, ...) doesn't depend on
   per-source config for any of the 14 connectors today. `configure()`
   mutates that same already-constructed connector in place once a real
   run's wire context arrives; `main.rs`'s job (construct + call
   `run_connector_main`) doesn't change at all. Only the 11 connectors
   that need no configuration change nothing — they inherit the no-op
   default.

5. **`RunContext` (the in-process, non-wire struct `open`/`fetch` see)
   does not gain a `config` field.** By the time `configure()` returns,
   the connector has already absorbed whatever it needed into its own
   `Config` fields — `open`/`fetch` keep reading `self.config.whatever`
   exactly as they do today, with no new parameter to thread through.

## Alternatives considered

**Pass the full `Config`/`SourceConfig` by reference into the
subprocess instead of a narrow options map.** Rejected — breaks the
process boundary's whole reason for existing (ADR-0001: "a subprocess
literally cannot read a [value] it wasn't handed"). `Config` also isn't,
and shouldn't become, `Serialize` — it holds host-only concerns (VPN
exec paths, webhook URLs, every other source's settings) a connector has
no business seeing. `sc.options` is already the least-privilege subset;
reuse it as-is.

**Host-side JSON-Schema validation of each connector's config before
spawn.** Rejected as unneeded complexity for round 1. Every connector
already self-validates its own inputs (secrets, and now config) inside
its own code and reports failure via `ConnectorError` — consistent with
how secret validation already works; no new validation framework needed
until a real pain point shows up (e.g. a config-editing UI wanting
pre-submit validation, which is out of scope here).

**Serialize `sc.options` as one opaque JSON string (reusing
`config_json` verbatim) instead of a structured `HashMap<String, Value>`
field.** Considered, since `config_json` is already computed. Rejected
for consistency: `WireRunContext` already uses structured typed fields
for everything else (`secrets` is `HashMap<String, String>`, not one
encoded blob) — a second string-encoding layer would just make every
connector's `configure()` do an extra `serde_json::from_str` for no
benefit.

**Give every connector's `Config` struct `#[derive(Deserialize)]` and
have `main.rs` merge the wire config into `Config::default()` directly,
skipping a new trait method entirely.** Attractive — no `Connector`
trait change at all. Rejected: the handshake must be written from an
already-constructed connector *before* the wire context is available
(see point 4 above), so construction with `Config::default()` has to
happen regardless — a post-construction mutation step is unavoidable
either way. Doing that mutation as connector-owned logic (a real method
returning a real `ConnectorError`) keeps per-field validation and
clear failure messages in the connector's own hands, rather than
accepting arbitrary JSON shapes into every `Config` struct with no
per-field checking.

## Consequences

- `mastodon`'s `instance`, `podcast`'s `feeds`, and (optionally)
  `bluesky`'s `identifier` become real once each implements `configure()`
  — no `dbs.toml`/`config.rs` parsing change needed; `RESERVED_SOURCE_KEYS`
  already carves out exactly the right non-reserved key space today.
- **Breaking, mechanically.** `WireRunContext` gaining a field is
  `#[serde(default)]`-safe on the wire (JSON), but every existing Rust
  struct-literal construction of `WireRunContext` — `raindrop`'s
  integration test plus all 13 from #164 (~14 files) — lists every field
  explicitly with no `..Default::default()` tail, so each needs one line
  added (`config: HashMap::new(),`) to keep compiling. Small, mechanical,
  zero behavior change for those tests — but real, and should land as
  part of the same PR that adds the field, not silently assumed away.
- `mastodon`'s and `podcast`'s subprocess integration tests (#164) should
  gain a further test sending a `config` map with the real
  `instance`/`feeds` key — proving the production wire path, not just
  the test-only `DBS_..._TEST_BASE_URL`/`DBS_PODCAST_TEST_FEED_URL` env
  var overrides #164 introduced for pointing at a mock server (an
  orthogonal concern: *where* a test points requests vs. *what* business
  config a real user supplies — `configure()` sits on top of whichever
  default `main.rs` already established, it doesn't replace it).
- Every other connector: zero code change, zero behavior change.
- **Follow-up issue:** implement this ADR (not bundled into this ADR's
  own PR, which is docs-only) — matches ADR-0001's precedent.
