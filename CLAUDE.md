# Rowlark

Fast, small, open-source, cross-platform desktop DB client (PostgreSQL, MySQL/MariaDB). Tauri 2 (Rust) + React + TypeScript + Vite. Full plan and decisions are in `PLAN.md`; read it before making architectural choices.

## About the owner

Strong in JavaScript/TypeScript, new to Rust. So:
- Keep Rust thin, simple and well commented (explain *why*, avoid clever macros and heavy lifetimes).
- Put product logic in TypeScript, not Rust.
- When Rust errors or concepts come up, explain them briefly in plain terms.

## Layout

- `crates/core`: drivers, `Value` enum, SSH, streaming. No Tauri or UI deps.
- `src-tauri`: Tauri app crate; commands are thin wrappers over `core`.
- `src/`: React + TypeScript UI.
- `docker-compose.yml`: Postgres 14–17, MySQL 8/9, MariaDB test servers.

## Rules

- **Type fidelity:** decimals and big ints cross IPC as strings; keep JSON and timestamps as original text; unknown types fall back to `Value::Unknown`. Never use f64 for decimals. Edits send text with an explicit server-side cast.
- **Large results:** stream in ~1k-row batches; cap the UI buffer; exports run in Rust, not through the UI; truncate large cells.
- **Drivers:** the UI only talks to the `Driver` trait and normalized model. Dialect and flavor quirks (MariaDB vs MySQL) stay inside drivers.
- **Secrets:** passwords only in the OS keychain (`keyring`), never in config files, logs or the local SQLite store.
- **Webview security:** keep the strict CSP in `tauri.conf.json`; never `dangerouslySetInnerHTML` or `eval`; render DB values and errors as text only; grant new Tauri capabilities sparingly. See PLAN.md §12.
- **Permissions and safety:** the database enforces permissions, not Rowlark. Never parse SQL to allow or block it, and never add app-side permission checks; run what the user types and show the server's answer or error. There is no read-only mode in Rowlark; to get one, the user connects with a database user that only has the privileges they want. The one app-side guard is a Confirm | Cancel prompt before DROP, TRUNCATE and DELETE/UPDATE without WHERE. It is a prompt only and never blocks.
- **No environment color tags:** no prod/staging/local dots or colors anywhere in the UI.
- **Platforms:** macOS first; Windows and Linux come later. Don't spend time on them yet, but nothing macOS-only unless gated by `cfg`. Avoid bleeding-edge web APIs (Linux WebKitGTK is the weakest webview).
- **Theme:** dark mode only, permanently. Use the CSS variables in `src/index.css`; no light theme or toggle.
- **License:** intentionally none for now (public, view-only). Don't add a LICENSE file or `license` fields until the owner decides.
- Order of work: `Value` + `Driver` + Docker tests first, UI after. New drivers or types need golden tests.

## Commands

- Dev: `npm run tauri dev`
- Build: `npm run tauri build`
- Rust tests: `cargo test --workspace`
- Test DBs: `docker compose up -d` (needs Docker or Colima running; with Colima the compose plugin may need registering)
- Rust format/lint: `cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings`
- Rust is pinned in `rust-toolchain.toml` (rustup installs it automatically)
- Frontend lint/typecheck: `npm run lint` / `npm run typecheck`

## Conventions

- Rust: `cargo fmt` and `cargo clippy` clean; errors via `thiserror`; no `unwrap()` in non-test code.
- TypeScript: strict mode, no `any`; functional React components with hooks.
- Small, focused commits. Do not commit unless asked.
- Don't add dependencies without a reason; the binary must stay small.
