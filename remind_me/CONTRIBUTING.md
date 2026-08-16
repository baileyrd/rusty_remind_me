# Contributing Guidelines

Thank you for contributing to `rusty_remind_me`! This document outlines our development workflows, coding standards, and testing procedures.

---

## 1. Development Setup

### Prerequisites
- **Rust Toolchain**: Rust 1.94+ with Cargo (`rust-version` in the workspace `Cargo.toml` — a demonstrated floor, not a bisected minimum; `rust-toolchain.toml` pins the newer version CI and developers actually build with).

### Local Workspace Setup
```bash
# Clone the repository
git clone https://github.com/baileyrd/rusty_remind_me
cd rusty_remind_me

# Validate cargo dependencies and path resolution
cargo check --workspace

# Run tests
cargo test --workspace
```

---

## 2. Code Quality & Standards

We enforce strict Rust idiom code quality across the workspace:

### Formatting & Linting
Run the following commands before submitting code:

```bash
# Format code according to Rust standard style
cargo fmt --all

# Run Clippy lints
cargo clippy --workspace -- -D warnings
```

### Key Coding Conventions
1. **Zero Warnings**: All code should compile cleanly without warnings.
2. **Error Handling**: Use explicit `thiserror` and `Result<T, E>` types instead of `.unwrap()` or `.expect()` in non-test code.
3. **Thread Safety**: Any database access across threads must acquire a lock via `db.conn()` (a `parking_lot::Mutex<Connection>` guard — see `ARCHITECTURE.md` §6). Most background work in this workspace is plain OS threads (`std::thread::Builder::spawn`), not `tokio` tasks; `remind_me_remote` is the one crate that runs on `tokio`.
4. **Preserve Comments & Docstrings**: Maintain architectural comments explaining mathematical formulas (e.g. ACT-R decay and RRF scoring).

---

## 3. Testing Standards

Every feature or bug fix must be accompanied by automated unit or integration tests.

### Running Test Suites
```bash
# Run unit tests across all crates
cargo test --workspace

# Run a specific test by name
cargo test test_database_creation_and_add_memory

# Run tests with output printed
cargo test --workspace -- --nocapture
```

### Test Locations
- **Unit Tests**: Placed inside module files within `src/` (e.g., `vitality.rs`, `retrieval.rs`) under `#[cfg(test)]`.
- **Integration Tests**: Placed in crate `tests/` directories (e.g., `crates/remind_me_core/tests/db_test.rs`).

---

## 4. The Rusty Mill Ecosystem — Not a Dependency Today

Every crate in this workspace once listed the `rusty_*` "Rusty Mill" crates
(`rusty_tokio`, `rusty-db`, `rusty_json`, `rusty-search`, `rusty_http`,
`rusty_lines`, `rusty_term`, `rusty_time`, `rusty_config`) as
`../Rusty_Mill/...` path dependencies against a monorepo that never existed
at those paths — the workspace failed to load, and not one source file
actually called into any of them. They were removed; see the "Rusty Mill
ecosystem dependencies" comment in the workspace `Cargo.toml` for the full
account.

Upstream, Rusty Mill is ~40 standalone repositories (`baileyrd/rusty_db`,
`baileyrd/rusty_json`, ...), not a monorepo. **Do not add a `rusty_*` crate
speculatively.** If a real call site needs a capability one of those repos
provides, add it as a git dependency at that point — not the whole suite up
front — and prefer it over an equivalent crates.io crate only once it is
actually pulled in and used.

---

## 5. Pull Request Checklist

Before submitting a Pull Request:
- [ ] `cargo check --workspace` compiles cleanly.
- [ ] `cargo test --workspace` passes all unit and integration tests.
- [ ] `cargo fmt --all` formats all code files.
- [ ] Documentation in `README.md` and `ARCHITECTURE.md` is updated if API signatures or CLI subcommands changed.
