# `rusty_recall`

One Cargo workspace holding two Rust products that were previously separate
repositories:

| Directory | Was | What it is |
| --------- | --- | ---------- |
| [`remind_me/`](./remind_me) | [`baileyrd/rusty_remind_me`](https://github.com/baileyrd/rusty_remind_me) | Persistent long-term memory engine and MCP server — SQLite FTS5 search, ACT-R vitality decay, RRF ranking, a knowledge-graph entity system, and a markdown wiki. Ships `rusty-remind-me` and `rusty-remind-me-hub`. |
| [`dbs/`](./dbs) | [`baileyrd/rusty_dbs`](https://github.com/baileyrd/rusty_dbs) | Incremental multi-source backup system — pulls a person's data out of 14 services into one local SQLite database, with a CLI and an optional web UI. Ships `dbs` plus 14 `dbs-connector-<type>` subprocess binaries. |

Each half keeps its own `README.md`, `ARCHITECTURE.md`, `CONTRIBUTING.md`,
`RELEASE_NOTES.md` and `docs/adr/`. Those are still the authoritative
documents for their half; this file only covers what the merge itself
changed.

## Why they are merged

The two already met at a seam. `remind_me`'s
[`dbs_import`](./remind_me/crates/remind_me_core/src/dbs_import.rs) reads a
`dbs` archive directly and turns each item into a memory with its source and
tags promoted to graph entities, and `remind_me wiki-import` is the ingestion
path for `dbs export-wiki`. Before the merge that seam spanned two
repositories, so a change to `dbs`'s schema and the `remind_me` reader that
consumes it could not land in one commit, and no CI run ever built both sides
together.

## Building

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

`rust-toolchain.toml` pins the compiler (1.97.0), so `rustup` selects it
automatically and CI and a local checkout lint identically.

To work on one half only, scope with `-p`:

```bash
cargo test -p remind_me_core
cargo test -p dbs-core
```

## Layout

```
Cargo.toml            the single [workspace]; all 25 crates are members
rust-toolchain.toml   pinned compiler, shared by both halves
LICENSE               MIT, covering both halves
.github/workflows/    ci.yml and release.yml, covering both halves
remind_me/            crates/, docs/, scripts/, commands/, hooks/, .claude-plugin/
dbs/                  crates/, docs/
```

The two trees keep their prefixes instead of being flattened into one
top-level `crates/`. Two reasons, both practical: the halves collide
extensively at the file level (each shipped a `README.md`,
`ARCHITECTURE.md`, `CONTRIBUTING.md`, `RELEASE_NOTES.md`, `gap-analysis.md`,
and a `docs/adr/` numbered from `0001`), and keeping the prefixes means the
grafted history still resolves against the paths its commits actually used.

## History

Neither repository's history was discarded and neither was made the trunk.
The root commit here is empty; `rusty_remind_me` (417 commits) and
`rusty_dbs` (273 commits) are each grafted on top with their trees relocated
under a prefix, giving 693 commits reachable from this branch.

The practical consequence is that history lookups need the *original* path,
not the current one:

```bash
# Works — blame follows the graft and reports the pre-merge path.
git blame remind_me/crates/remind_me_core/src/dbs_import.rs

# Works — note the path has no remind_me/ prefix, and --full-history is
# required to stop git's merge simplification from pruning the grafted side.
git log --full-history -- crates/remind_me_core/src/dbs_import.rs

# Does NOT reach pre-merge commits: --follow cannot cross the graft.
git log --follow -- remind_me/crates/remind_me_core/src/dbs_import.rs
```

## What the merge deliberately did not change

- **Two databases, not one.** `remind_me` keeps `~/.remind-me/memory.db` at
  its own schema version, which is the version upstream `remind-me` reports —
  a mismatch there makes the database silently unreadable to it. `dbs` keeps
  its own archive. `dbs_import` still reads that archive `SQLITE_OPEN_READ_ONLY`
  with plain SQL and no Cargo dependency on `dbs-core`, so this workspace
  cannot damage a backup archive even by accident.
- **Connectors are still subprocesses.** Sharing a workspace does not make
  them in-process trait objects; see
  [`dbs/docs/adr/0001`](./dbs/docs/adr/0001-dynamic-plugin-registry.md).
- **Both async stacks stay.** `remind_me` is synchronous except
  `remind_me_remote`; `dbs-web` is axum/tokio. Nothing was rewritten to
  converge them.

## Known follow-ups

- **`axum` and `reqwest` are each compiled twice** — `remind_me_remote` is on
  axum 0.8 / reqwest 0.13, `dbs-web` and `dbs-core` on axum 0.7 / reqwest
  0.12. This costs build time and nothing else: neither is a `links` crate, so
  the versions coexist safely. Unifying means porting `dbs-web` across axum's
  0.7→0.8 routing changes, which is a behavioral change to the web UI and was
  deliberately kept out of the merge. (`rusqlite` was *not* left split — see
  the note in the root `Cargo.toml`; `libsqlite3-sys` declares
  `links = "sqlite3"`, so two versions of it is a hard error rather than an
  inefficiency, and both halves are on 0.32.)
- **`dbs-connector-youtube`'s tests carry a latent `ETXTBSY` race, and this
  workspace makes it more likely to fire.** Several of them write a fake
  `yt-dlp` shell script and immediately exec it; in a multithreaded program,
  another thread's `fork`/`exec` can still hold a write handle on that file,
  and the exec fails with `Text file busy` (`os error 26`). It is unrelated
  to any dependency version — it is an OS-level exec error — and the tests
  pass reliably when the crate is run on its own. What changed is scheduling
  pressure: `cargo test --workspace` here runs 25 crates' test binaries
  concurrently rather than 19, and it surfaced once in a full run (
  `a_failed_list_withholds_the_reconcile_marker_but_keeps_the_other_list`).
  The fix is on the test side — serialize the tests that exec a
  just-written script — and was left out of the merge because it is a change
  to how that half tests itself, not to how the two halves fit together.
- **`release.yml` has not been exercised end to end.** It now builds and
  packages all 17 binaries rather than the original 2, and renames the asset
  from `rusty-remind-me-v*` to `rusty-recall-v*`. Nothing proves that until
  the first version bump lands on `main`.

## License

MIT — see [LICENSE](./LICENSE). This is a change for the `dbs/` half, which
declared `UNLICENSED` as a separate repository; neither repository ever
shipped a license file.
