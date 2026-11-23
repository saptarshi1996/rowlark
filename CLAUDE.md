# Rowlark

Fast, small, open-source desktop DB client (PostgreSQL, MySQL/MariaDB). 100% Rust: native GPU-rendered UI with GPUI, no webview, no JavaScript. Full plan and decisions are in `PLAN.md`; read it before making architectural choices.

## About the owner

Strong in JavaScript/TypeScript, learning Rust as the project goes. So:
- Keep Rust simple and well commented (explain *why*; avoid clever macros, heavy generics and lifetime puzzles).
- When Rust errors or concepts come up, explain them briefly in plain terms.
- Prefer boring, readable code over the most idiomatic-but-dense version.

## Layout

- `crates/core`: drivers, `Value` enum, SSH, streaming. No UI deps, so it is testable alone and the UI stays swappable.
- UI crate (GPUI): added after `Value` + `Driver` exist, once a small spike proves GPUI builds on our pinned toolchain (see PLAN.md Phase 0b).
- `docker-compose.yml`: Postgres 14–17, MySQL 8/9, MariaDB test servers.

## Rules

- **Type fidelity:** decimals and big ints stay exact (`Value::Decimal(String)`, never f64); keep JSON and timestamps as original text; unknown types fall back to `Value::Unknown`. Edits send text with an explicit server-side cast.
- **Large results:** stream from the server cursor in ~1k-row batches; never wrap or rewrite the user's SQL in LIMIT/OFFSET to page it; the grid keeps a bounded window of rows and fetches more as the user scrolls; exports run straight from the driver to the file; truncate large cells.
- **Drivers:** the UI only talks to the `Driver` trait and normalized model. Dialect and flavor quirks (MariaDB vs MySQL) stay inside drivers. Drivers are compiled in; no plugin system.
- **Secrets:** passwords only in the OS keychain (`keyring`), never in config files, logs or the local SQLite store.
- **Untrusted data:** DB values, errors and schema names are plain text everywhere; never execute or interpret them. Grant no more OS access than needed. See PLAN.md §12.
- **Permissions and safety:** the database enforces permissions, not Rowlark. Never parse SQL to allow or block it, and never add app-side permission checks; run what the user types and show the server's answer or error. There is no read-only mode in Rowlark; to get one, the user connects with a database user that only has the privileges they want. The one app-side guard is a Confirm | Cancel prompt before DROP, TRUNCATE and DELETE/UPDATE without WHERE. It is a prompt only and never blocks.
- **No environment color tags:** no prod/staging/local dots or colors anywhere in the UI.
- **Platforms:** macOS on Apple Silicon first; Windows and Linux come later. Don't spend time on them yet, but nothing macOS-only unless gated by `cfg`.
- **Theme:** dark mode only, permanently. All colors come from one theme module in the UI crate; no light theme or toggle.
- **License:** intentionally none for now (public, view-only). Don't add a LICENSE file or `license` fields until the owner decides.
- Order of work: `Value` + `Driver` + Docker tests first, UI after. New drivers or types need golden tests.

## Commands

- Rust tests: `cargo test --workspace`
- Test DBs: `docker compose up -d` (needs Docker or Colima running; with Colima the compose plugin may need registering)
- Rust format/lint: `cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings`
- Rust is pinned in `rust-toolchain.toml` (rustup installs it automatically)
- Run the app: `cargo run -p <ui-crate>` (crate does not exist yet)

## Conventions

- Rust: `cargo fmt` and `cargo clippy` clean; errors via `thiserror`; no `unwrap()` in non-test code.
- Small, focused commits. Do not commit unless asked.
- Don't add dependencies without a reason; the binary must stay small.
