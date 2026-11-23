# Rowlark — Plan

A fast, small, open-source, cross-platform desktop database client for **PostgreSQL** and **MySQL/MariaDB**.

## 1. Why

- Many existing clients are JVM-based: large and slow to start.
- Polished native clients are often paid.
- Web-based clients are awkward for daily desktop use.
- Dedicated Postgres or MySQL clients only handle one engine.

Goal: a polished, keyboard-first client that is free, open source, ~10–15 MB, and starts in under a second.

## 2. Decisions

| Topic | Decision |
|---|---|
| Name | **Rowlark** (registry names free on crates.io/npm; verify GitHub/web before public release) |
| Shell + backend | **Tauri 2** (Rust). Not Electron (too big/heavy) |
| UI | **React + TypeScript + Vite** |
| Grid | Canvas-based virtualized grid (Glide Data Grid) |
| SQL editor | CodeMirror 6 |
| Platforms | macOS on Apple Silicon only (the owner's only build/test device); Intel Macs, Windows and Linux later, still the long-term goal |
| First engines | PostgreSQL, MySQL, MariaDB (variant of MySQL) |
| License | None for now: repo is public but view-only (all rights reserved). Decide later |
| Theme | Dark mode only, permanently. No light theme |

Owner profile: strong in JavaScript, new to Rust. So **keep Rust thin and well commented; keep product logic in TypeScript.**

## 3. Architecture

```
┌─────────────── UI (React + TypeScript) ──────────────────────────┐
│ Connection manager │ Schema tree │ SQL editor tabs │ Data grid   │
└──────────────────────────── Tauri IPC ───────────────────────────┘
┌─────────────────────────── Rust ─────────────────────────────────┐
│ crates/core (no UI deps)                                         │
│   trait Driver { connect, query_stream, cancel, introspect, … }  │
│    ├─ PostgresDriver  (tokio-postgres)                           │
│    └─ MySqlDriver     (mysql_async)                              │
│   Value enum │ SSH tunnel (russh) │ Keychain (keyring)           │
│ src-tauri: Tauri commands, thin wrappers over core               │
└──────────────────────────────────────────────────────────────────┘
```

- Rust owns: connections, streaming, cancel, SSH, TLS, secrets, type conversion.
- TypeScript owns: UI, state, edit-to-SQL generation, autocomplete, export formats, shortcuts.
- The UI only sees a normalized model (databases, schemas, tables, columns, indexes, FKs, `Value`). Dialect differences stay inside drivers.
- Local app data (saved connections, history, snippets) lives in a SQLite file; passwords live in the OS keychain only.

## 4. Type fidelity (hardest part)

One `Value` enum crosses the IPC boundary; drivers convert into it.

```rust
enum Value {
  Null, Bool(bool), Int(i64), UInt(u64), Float(f64),
  Decimal(String),                 // exact digits, never f64
  Text(String), Bytes(Vec<u8>),
  Json(String),                    // original text (key order, number precision)
  Timestamp { iso: String, tz: Option<String> },
  Date(String), Time(String), Uuid(String),
  Array(Vec<Value>),
  Unknown { type_name: String, text: String },
}
```

Rules:
- Decimals and big integers travel as **strings** (JS numbers lose precision above 2^53).
- Keep original text for JSON and timestamps; parse only for display.
- Unrecognized types fall back to `Unknown` with the server's text form.
- Edits send values as text with an explicit server-side cast (e.g. `$1::numeric`); never reformat client-side.
- **Golden tests:** per DB, one table with every type and edge values (NULL, min/max ints, `0.1`, tiny decimals, DST timestamps, emoji, huge JSON). Read, write back, read again, assert unchanged.

## 5. Large results

- Rust streams rows in batches of ~1,000 over a Tauri `Channel`.
- UI keeps a capped window (~100k rows) with "load more / export all".
- Export runs fully in Rust (DB → file), bypassing the UI.
- Large cells (text/blob) truncated to ~1 KB in the grid; full value loaded on demand.
- Cancel stops the stream **and** the server-side query (`pg_cancel_backend`, `KILL QUERY`).

## 6. Remote connections (designed in from the start, built after local works)

**Scope order:** first target is **local MySQL and local Postgres**, including databases running in Docker containers (host + mapped port). Cloud services, SSH tunnels and TLS come after local is solid. Keep the connection model (host, port, user, TLS options, tunnel options) extensible so adding them later needs no rewrite.

- TLS modes: disable / require / verify-ca / verify-full, custom CA, client cert/key.
- SSH tunnel (`russh`): key, password, agent; jump hosts. Driver connects to the local forwarded port.
- Paste a connection URL (`postgres://…`, `mysql://…`).
- Timeouts, keepalive, graceful auto-reconnect.
- One dedicated connection per editor tab (transactions/session state); separate pool for schema browsing.
- Read-only mode and environment color tags (red = prod).
- Works with managed/cloud-hosted databases (TLS and SSH tunnel support). Target services: AWS RDS/Aurora, Google Cloud SQL, Azure Database, Neon, Supabase, PlanetScale.

## 7. Flavor and test matrix

- On connect, run `SELECT VERSION()` and store `Flavor::{Postgres, MySql, MariaDb}` plus version. Keep quirks (information_schema differences, JSON-as-LONGTEXT in MariaDB, etc.) inside the driver.
- `docker-compose.yml`: Postgres 14–17, MySQL 8/9, MariaDB 11, each on its own port. MySQL 5.7 is dropped for now (no arm64 image, slow under emulation on Apple Silicon); add MariaDB 10 or MySQL 5.7 later if needed.
- The same integration suite runs against every service. Locally it uses Docker (Colima works). CI is macOS-only for now (macOS runners have no Docker), so the DB suite runs locally only until Linux returns (see open questions).

## 8. Cross-platform notes

- macOS: WKWebView. Windows: WebView2 (Chromium). Linux: WebKitGTK (weakest; test early).
- Use a canvas grid so scroll performance is consistent. Avoid bleeding-edge web APIs.
- Secrets via `keyring` (Keychain / Credential Manager / Secret Service).
- CI builds on macOS for now; add Windows and Linux to the matrix when we start on them (`tauri-action`).

## 9. Roadmap

**Phase 0 — Setup (2–3 days)**
Tauri 2 + React + TS + Vite; Rust workspace (`crates/core`, `src-tauri`); CI matrix; `docker-compose.yml`.

**Phase 1 — Types, drivers, connect and query (1–2 wk)**
`Value`, `Driver` trait, streaming API **before any UI**; Postgres + MySQL drivers; golden type tests passing against the Docker test databases; connection manager for **local/Docker** connections (host, port, user, password, URL paste, test connection, keychain); run a query and stream rows into the grid.

**Phase 1b — Remote and cloud (after local is solid)**
TLS modes and certificates, SSH tunnels and jump hosts, keepalive/reconnect, then verify against RDS/Aurora, Cloud SQL, Azure, Neon, Supabase, PlanetScale.

**Phase 2 — Daily driver (2–3 wk)**
Lazy schema tree; multi-tab editor (⌘Enter run selection, cancel, history); table browser with server-side sort/filter/pagination; export CSV/JSON/SQL.

**Phase 3 — Editing and safety (2 wk)**
Inline edit/insert/delete by primary key with staged-changes SQL preview; explicit transactions; prod safeguards; DDL viewer; structure and index view.

**Phase 4 — Power features (3+ wk)**
Schema-aware autocomplete; EXPLAIN visualizer; ER diagram; CSV import; saved queries; DB-to-DB copy.

**Phase 5 — Release**
Code signing/notarization (macOS), MSI/NSIS (Windows), AppImage/deb (Linux), auto-update, README with screenshots. Then more engines (SQLite, MSSQL, ClickHouse, Redis).

## 10. Setup status / next steps

- [x] Node, npm, Docker, Xcode CLT installed
- [x] Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- [x] Rename project folder `dbpoc` → `rowlark`, restart Claude Code from it
- [x] `git init`
- [x] Phase 0 scaffold
- [ ] Phase 1 (`Value` + `Driver` + Docker + golden tests first, UI after)

## 11. Open questions

- License: none for now (view-only). Pick one before inviting use or contributions.
- CI for the DB tests: deferred. macOS only, no Linux or Windows jobs, so the Docker-based DB suite runs locally only. Revisit when Linux comes back.
- Verify the name "Rowlark" on GitHub/web/app stores before going public.
- Package manager: npm (installed) vs pnpm.
- Encrypt query history and saved connection metadata at rest, or rely on OS disk encryption (FileVault)?

## 12. Security

**Principles:** the database and its data are untrusted input; the webview is a thin UI with the least privilege we can give it; secrets never touch disk in plain text.

- **Secrets:** passwords only in the macOS Keychain (`keyring`). Never in config, logs, SQLite or crash output. Passwords cross IPC once (save) and are never sent back to the UI.
- **Webview hardening:** strict CSP (set in `tauri.conf.json`; `devCsp` is looser only for Vite HMR). No `dangerouslySetInnerHTML` and no `eval`; cell values, error messages and schema names are always rendered as text.
- **Tauri permissions:** capabilities grant only what the UI needs (currently `core:default`). Review every new command and plugin; no shell, no broad filesystem access.
- **Connections:** local and Docker may skip TLS. Remote connections default to verified TLS. SSH tunnels must check host keys (known-hosts, trust-on-first-use prompt, warn on change). No silent downgrade from verified to unverified.
- **Query safety:** confirm destructive statements (DELETE/UPDATE without WHERE, DROP, TRUNCATE); per-connection read-only mode; prod color tags; edits only by primary key with a SQL preview. User SQL runs as written; we never build SQL by string-pasting values (use parameters or explicit casts).
- **Local data:** saved connections and query history live in SQLite. Add a "don't record history" option and a retention limit; history may contain sensitive literals. Exports are written only where the user chooses.
- **Supply chain:** CI runs `npm audit` and `cargo audit`; commit lockfiles; add dependencies only with a reason; pin the Rust toolchain.
- **Release (Phase 5):** code signing and notarization; signed auto-updates (updater keys kept offline); publish checksums.
- **Reporting:** add a `SECURITY.md` with a private reporting contact before inviting outside use.

Known gap: query history encryption at rest is undecided (see open questions).
