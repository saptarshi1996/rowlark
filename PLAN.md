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
| Platforms | macOS first (current focus); Windows and Linux later, still the long-term goal |
| First engines | PostgreSQL, MySQL, MariaDB (variant of MySQL) |
| License | MIT or Apache-2.0 (decide before first public push) |

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
│ crates/app: Tauri commands, thin wrappers over core              │
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
- `docker-compose.yml`: Postgres 14–17, MySQL 5.7/8/9, MariaDB 10/11, each on its own port.
- The same integration suite runs against every service. CI runs it on Linux.

## 8. Cross-platform notes

- macOS: WKWebView. Windows: WebView2 (Chromium). Linux: WebKitGTK (weakest; test early).
- Use a canvas grid so scroll performance is consistent. Avoid bleeding-edge web APIs.
- Secrets via `keyring` (Keychain / Credential Manager / Secret Service).
- CI builds on macOS for now; add Windows and Linux to the matrix when we start on them (`tauri-action`).

## 9. Roadmap

**Phase 0 — Setup (2–3 days)**
Tauri 2 + React + TS + Vite; Rust workspace (`core`, `app`); CI matrix; `docker-compose.yml`.

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

- License: MIT vs Apache-2.0.
- Verify the name "Rowlark" on GitHub/web/app stores before going public.
- Package manager: npm (installed) vs pnpm.
