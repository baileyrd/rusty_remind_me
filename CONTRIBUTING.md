# Contributing Guidelines

Thank you for contributing to `rusty_remind_me`! This document outlines our development workflows, coding standards, testing procedures, and integration practices with the **Rusty Mill** monorepo.

---

## 1. Development Setup

### Prerequisites
- **Rust Toolchain**: Rust 1.75+ with Cargo.
- **Rusty Mill Ecosystem**: Ensure `c:\dev\Rusty_Mill` (or relative path `../Rusty_Mill`) is present.

### Local Workspace Setup
```bash
# Clone the repository adjacent to Rusty_Mill
cd c:\dev\rusty_remind_me

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
3. **Thread Safety**: Any database access across async tasks must acquire a lock via `db.conn()`.
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

## 4. Contributing Back to Rusty Mill

`rusty_remind_me` is built directly on the **Rusty Mill** monorepo (`c:\dev\Rusty_Mill`).

### Dependency Rule
- **Primary Dependencies**: Always check if a capability is provided by a `Rusty_Mill` crate (`rusty_tokio`, `rusty-db`, `rusty_json`, `rusty-search`, `rusty_http`, `rusty_lines`, `rusty_term`, `rusty_time`, `rusty_config`) before adding third-party crates from crates.io.
- **Addressing Gaps**: If a feature is missing in a Rusty Mill crate (such as custom SQLite bindings or FTS5 query extensions), update the crate directly inside `c:\dev\Rusty_Mill/<crate_name>` and link the workspace path in `Cargo.toml`.

---

## 5. Pull Request Checklist

Before submitting a Pull Request:
- [ ] `cargo check --workspace` compiles cleanly.
- [ ] `cargo test --workspace` passes all unit and integration tests.
- [ ] `cargo fmt --all` formats all code files.
- [ ] Documentation in `README.md` and `ARCHITECTURE.md` is updated if API signatures or CLI subcommands changed.
