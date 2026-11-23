# Rowlark

Fast, small, open-source, cross-platform desktop DB client (PostgreSQL, MySQL/MariaDB). Tauri 2 (Rust) + React + TypeScript + Vite. Full plan and decisions are in `PLAN.md`; read it before making architectural choices.

## About the owner

Strong in JavaScript/TypeScript, new to Rust. So:
- Keep Rust thin, simple and well commented (explain *why*, avoid clever macros and heavy lifetimes).
- Put product logic in TypeScript, not Rust.
- When Rust errors or concepts come up, explain them briefly in plain terms.

## Layout (target)

- `crates/core`: drivers, `Value` enum, SSH, streaming. No Tauri or UI deps.
- `crates/app` (or `src-tauri`): Tauri commands; thin wrappers over `core`.
- `src/`: React + TypeScript UI.
- `docker-compose.yml`: Postgres 14–17, MySQL 5.7/8/9, MariaDB test servers.

## Rules

- **Type fidelity:** decimals and big ints cross IPC as strings; keep JSON and timestamps as original text; unknown types fall back to `Value::Unknown`. Never use f64 for decimals. Edits send text with an explicit server-side cast.
- **Large results:** stream in ~1k-row batches; cap the UI buffer; exports run in Rust, not through the UI; truncate large cells.
- **Drivers:** the UI only talks to the `Driver` trait and normalized model. Dialect and flavor quirks (MariaDB vs MySQL) stay inside drivers.
- **Secrets:** passwords only in the OS keychain (`keyring`), never in config files, logs or the local SQLite store.
- **Safety:** destructive actions (DELETE/UPDATE without WHERE, DROP) need confirmation; respect per-connection read-only mode.
- **Cross-platform:** nothing macOS-only unless gated by `cfg`. Avoid bleeding-edge web APIs (Linux WebKitGTK is the weakest webview).
- Order of work: `Value` + `Driver` + Docker tests first, UI after. New drivers or types need golden tests.

## Commands (fill in as the scaffold lands)

- Dev: `npm run tauri dev`
- Build: `npm run tauri build`
- Rust tests: `cargo test --workspace`
- Test DBs: `docker compose up -d`
- Frontend lint/typecheck: `npm run lint` / `npm run typecheck`

## Conventions

- Rust: `cargo fmt` and `cargo clippy` clean; errors via `thiserror`; no `unwrap()` in non-test code.
- TypeScript: strict mode, no `any`; functional React components with hooks.
- Small, focused commits. Do not commit unless asked.
- Don't add dependencies without a reason; the binary must stay small.
