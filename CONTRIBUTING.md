# Contributing to `rusty_recall`

This repository holds two products in one Cargo workspace. Each half keeps
the contributing guide it had as a standalone repository, and those remain
authoritative for anything specific to that half:

- [`remind_me/CONTRIBUTING.md`](./remind_me/CONTRIBUTING.md) — the memory
  engine and MCP server.
- [`dbs/CONTRIBUTING.md`](./dbs/CONTRIBUTING.md) — the backup system and its
  connectors.

What follows is only what the merge added on top.

## Everything runs from the workspace root

There is one `[workspace]`, one `Cargo.lock`, and one `target/`. Run the
gates from the repository root, not from inside `remind_me/` or `dbs/`:

```bash
cargo fmt --all --check
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

`rust-toolchain.toml` pins the compiler, so `rustup` selects it for you and
your clippy run matches CI's rather than depending on what you happen to have
installed.

While iterating, scope to the crate you are touching — the full workspace is
25 crates and builds 17 binaries:

```bash
cargo test -p dbs-core
cargo test -p remind_me_core
```

## A change that crosses the two halves

This is the reason the repositories were merged, so it is worth stating
plainly: `remind_me`'s
[`dbs_import`](./remind_me/crates/remind_me_core/src/dbs_import.rs) reads a
`dbs` archive with hand-written SQL against `dbs`'s `items`/`sources` schema,
and there is deliberately no Cargo dependency between them. Nothing in the
compiler will tell you that a change to that schema broke the reader.

If you change `dbs`'s storage schema, run `cargo test -p remind_me_core` in
the same change and update `dbs_import.rs` in the same commit. That is now
possible in one commit and one CI run; before the merge it was not.

## Adding a crate

Add it under `remind_me/crates/` or `dbs/crates/` depending on which half it
belongs to, and add its path to `members` in the root `Cargo.toml`. Do not
add a `[workspace]` table to it — a nested workspace under this one silently
splits the build back into two.

## Dependency versions

Shared dependency versions live in the root `Cargo.toml`'s
`[workspace.dependencies]`. Before adding a second version of something that
is already there, check whether the crate is a `-sys` crate declaring
`links = "..."`: two versions of one of those is a hard build error rather
than an inefficiency. `rusqlite`/`libsqlite3-sys` is the one this workspace
already had to resolve, and the root `Cargo.toml` records why.

## Documentation

Docs for one half stay in that half — including its `docs/adr/`, which is
why both `remind_me/docs/adr/0001-*` and `dbs/docs/adr/0001-*` exist and
neither was renumbered. Only documentation about the merged repository itself
belongs at the root.
