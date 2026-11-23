# Rowlark — Plan

A fast, small, open-source, cross-platform desktop database client for **PostgreSQL** and **MySQL/MariaDB**.

## 1. Why

- Many existing clients are JVM-based: large and slow to start.
- Polished native clients are often paid.
- Web-based and Electron/webview clients are heavy or awkward for daily desktop use.
- Dedicated Postgres or MySQL clients only handle one engine.

Goal: a polished, keyboard-first client that is free, open source, small, and starts in under a second. Speed is the main target, so the whole stack is native Rust.

## 2. Decisions

| Topic | Decision |
|---|---|
| Name | **Rowlark** (registry names free on crates.io; verify GitHub/web before public release) |
| Language | **100% Rust.** No webview, no JavaScript, no IPC. Decided 2026-11-24 for speed: no serialization hop between database and screen, GPU-drawn grid |
| UI | **GPUI** (the GPU UI framework behind the Zed editor), likely with `gpui-component` for widgets. To be confirmed by the Phase 0b spike |
| Grid | Custom virtualized grid on GPUI: only visible rows are drawn, rows come from a bounded in-memory window |
| SQL editor | To decide in the spike (GPUI component editor, tree-sitter highlighting). Autocomplete is built in Rust, schema-aware |
| Platforms | macOS on Apple Silicon only (the owner's only build/test device); Intel Macs, Windows and Linux later, still the long-term goal |
| First engines | PostgreSQL, MySQL, MariaDB (variant of MySQL). Drivers are compiled in; no plugin system, more engines are added as new drivers |
| License | None for now: repo is public but view-only (all rights reserved). Decide later |
| Theme | Dark mode only, permanently. No light theme |

Owner profile: strong in JavaScript, learning Rust as we go. So **keep Rust simple and well commented; explain Rust concepts when they come up.**

Why not Tauri + React (the earlier plan): query rows would cross a Rust → webview JSON bridge, which is the main slowness other Tauri-based clients report, and the webview limits grid smoothness (worst on Linux). Native removes both. The cost is a younger UI framework, a UI written in Rust, and rougher Windows/Linux support later. `crates/core` has no UI dependencies, so the UI could be swapped again without touching the drivers.

## 3. Architecture

```
┌───────────────────────── UI crate (GPUI) ────────────────────────┐
│ Connection manager │ Schema tree │ SQL editor tabs │ Data grid   │
└────────────────────── plain Rust calls / channels ───────────────┘
┌─────────────────────────── crates/core ──────────────────────────┐
│   trait Driver { connect, query_stream, cancel, introspect, … }  │
│    ├─ PostgresDriver  (tokio-postgres)                           │
│    └─ MySqlDriver     (mysql_async)                              │
│   Value enum │ SSH tunnel (russh) │ Keychain (keyring)           │
└──────────────────────────────────────────────────────────────────┘
```

- `crates/core` owns: connections, streaming, cancel, SSH, TLS, secrets, type conversion, edit-to-SQL generation, export.
- The UI crate owns: windows, state, rendering, shortcuts, theme.
- The UI only sees a normalized model (databases, schemas, tables, columns, indexes, FKs, `Value`). Dialect differences stay inside drivers.
- Database work runs on a background async runtime (tokio); the UI thread never waits on the network. Rows reach the UI through channels in batches.
- Local app data (saved connections, history, snippets) lives in a SQLite file; passwords live in the OS keychain only.

## 4. Type fidelity (hardest part)

One `Value` enum is the only type the UI sees; drivers convert into it.

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
- Decimals stay as exact strings; never convert to f64.
- Keep original text for JSON and timestamps; parse only for display.
- Unrecognized types fall back to `Unknown` with the server's text form.
- Edits send values as text with an explicit server-side cast (e.g. `$1::numeric`); never reformat client-side.
- **Golden tests:** per DB, one table with every type and edge values (NULL, min/max ints, `0.1`, tiny decimals, DST timestamps, emoji, huge JSON). Read, write back, read again, assert unchanged.
- Follow-up: `Value` currently derives JSON serialization for the old IPC boundary. With no IPC it is only needed for export/debug output; decide whether to keep `serde` in core.

## 5. Large results

- Rows come from the server cursor and are sent to the UI in batches of ~1,000. The user's SQL is never wrapped or rewritten with LIMIT/OFFSET, so paging cannot break on odd SQL (some other clients had exactly that bug).
- The grid keeps a bounded window of rows in memory (cap TBD, ~100k) and shows "load more / export all"; it draws only the visible rows.
- Export runs fully in Rust, driver → file, never through the grid.
- Large cells (text/blob) truncated to ~1 KB in the grid; full value loaded on demand.
- Cancel stops the stream **and** the server-side query (`pg_cancel_backend`, `KILL QUERY`).
- Table browser mode (not free-form SQL) may page server-side with a primary-key-ordered query we build ourselves.

## 6. Remote connections (designed in from the start, built after local works)

**Scope order:** first target is **local MySQL and local Postgres**, including databases running in Docker containers (host + mapped port). Cloud services, SSH tunnels and TLS come after local is solid. Keep the connection model (host, port, user, TLS options, tunnel options) extensible so adding them later needs no rewrite.

- TLS modes: disable / require / verify-ca / verify-full, custom CA, client cert/key.
- SSH tunnel (`russh`): key, password, agent; jump hosts. Driver connects to the local forwarded port.
- Paste a connection URL (`postgres://…`, `mysql://…`).
- Timeouts, keepalive, graceful auto-reconnect.
- One dedicated connection per editor tab (transactions/session state); separate pool for schema browsing.
- No app-side read-only mode and no environment color tags (see §12).
- Works with managed/cloud-hosted databases (TLS and SSH tunnel support). Target services: AWS RDS/Aurora, Google Cloud SQL, Azure Database, Neon, Supabase, PlanetScale.

## 7. Flavor and test matrix

- On connect, run `SELECT VERSION()` and store `Flavor::{Postgres, MySql, MariaDb}` plus version. Keep quirks (information_schema differences, JSON-as-LONGTEXT in MariaDB, etc.) inside the driver.
- `docker-compose.yml`: Postgres 14–17, MySQL 8/9, MariaDB 11, each on its own port. MySQL 5.7 is dropped for now (no arm64 image, slow under emulation on Apple Silicon); add MariaDB 10 or MySQL 5.7 later if needed.
- The same integration suite runs against every service. Locally it uses Docker (Colima works). CI is macOS-only for now (macOS runners have no Docker), so the DB suite runs locally only until Linux returns (see open questions).

## 8. Cross-platform notes

- GPUI targets macOS (Metal), Windows and Linux. macOS is the best-supported; Windows and Linux support exists but is younger (other GPUI apps report: Linux Wayland-first, X11 not guaranteed, unsigned Windows builds). Revisit when we start on them.
- Secrets via `keyring` (Keychain / Credential Manager / Secret Service).
- CI builds on macOS for now; add Windows and Linux to the matrix when we start on them (Linux will need extra system libraries for the UI).

## 9. Roadmap

**Phase 0 — Setup (done)**
Rust workspace (`crates/core`), CI, `docker-compose.yml`. The earlier Tauri + React scaffold was removed on 2026-11-24 when the project went fully native.

**Phase 1 — Types, drivers, connect and query (1–2 wk)**
`Value`, `Driver` trait, streaming API **before any UI**; Postgres + MySQL drivers; golden type tests passing against the Docker test databases; connection model for **local/Docker** connections (host, port, user, password, URL parse, test connection, keychain).

**Phase 0b — UI spike (after the first driver works, 2–3 days)**
Prove GPUI builds on the pinned toolchain; open a dark window; draw a virtualized grid over 500k fake rows and check scroll smoothness and memory; pick the editor approach. Output: go/no-go on GPUI plus the UI crate skeleton. If GPUI blocks us, fall back options are listed in §11.

**Phase 1c — First screens**
Connection manager; run a query and stream rows into the grid.

**Phase 1b — Remote and cloud (after local is solid)**
TLS modes and certificates, SSH tunnels and jump hosts, keepalive/reconnect, then verify against RDS/Aurora, Cloud SQL, Azure, Neon, Supabase, PlanetScale.

**Phase 2 — Daily driver (2–3 wk)**
Lazy schema tree; multi-tab editor (⌘Enter run selection, cancel, history); table browser with server-side sort/filter/pagination; export CSV/JSON/SQL.

**Phase 3 — Editing and safety (2 wk)**
Inline edit/insert/delete by primary key with staged-changes SQL preview; explicit transactions; confirmation prompts for destructive statements; DDL viewer; structure and index view.

**Phase 4 — Power features (3+ wk)**
Schema-aware autocomplete; EXPLAIN visualizer; ER diagram; CSV import; saved queries; DB-to-DB copy.

**Phase 5 — Release**
Code signing/notarization (macOS), then Windows and Linux packages, auto-update, README with screenshots. Then more engines (SQLite, MSSQL, ClickHouse, Redis) as in-tree drivers.

## 10. Setup status / next steps

- [x] Docker, Xcode CLT installed
- [x] Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- [x] Rename project folder `dbpoc` → `rowlark`, restart Claude Code from it
- [x] `git init`
- [x] Phase 0 scaffold (Tauri + React, since removed)
- [x] Decision: fully native Rust UI (2026-11-24); web UI and Tauri shell removed
- [ ] Phase 1 (`Driver` trait + Postgres/MySQL drivers + Docker + golden tests first, UI after)
- [ ] Phase 0b UI spike

## 11. Open questions

- GPUI risk: young framework, thin docs, API changes between versions. Mitigation: pin versions, keep `crates/core` UI-free, run the spike before committing to screens. Alternatives if it fails: another native Rust toolkit (egui, iced, Slint), or return to a webview shell over the same core.
- Which crate set for GPUI (`gpui` from crates.io or git, `gpui-component`)? Verify in the spike.
- SQL editor: build on a GPUI component editor vs. something custom; syntax highlighting via tree-sitter.
- Keep `serde`/JSON on `Value` or drop it (see §4)?
- License: none for now (view-only). Pick one before inviting use or contributions. Note GPUI components are Apache-2.0; check compatibility when choosing.
- CI for the DB tests: deferred. macOS only, no Linux or Windows jobs, so the Docker-based DB suite runs locally only. Revisit when Linux comes back.
- Verify the name "Rowlark" on GitHub/web/app stores before going public.
- Encrypt query history and saved connection metadata at rest, or rely on OS disk encryption (FileVault)?

## 12. Security

**Principles:** the database and its data are untrusted input; the app asks the OS for the least it needs; secrets never touch disk in plain text.

- **Secrets:** passwords only in the macOS Keychain (`keyring`). Never in config, logs, SQLite or crash output. Once saved, the password is read from the keychain only when connecting and is never shown back in the UI.
- **Untrusted data:** cell values, error messages and schema names are always treated as plain text: never executed, never interpreted as markup or commands. With no webview there is no HTML/JS injection surface, which removes a whole class of risk.
- **OS access:** no shell execution on behalf of data, no broad filesystem access. Exports are written only where the user chooses. macOS sandbox/entitlements get reviewed at release time.
- **Connections:** local and Docker may skip TLS. Remote connections default to verified TLS. SSH tunnels must check host keys (known-hosts, trust-on-first-use prompt, warn on change). No silent downgrade from verified to unverified.
- **The database enforces permissions.** Rowlark does not parse SQL, block statements or keep its own permission model. It runs what the user types and shows the server's result or error (for example "permission denied for table orders"). Optionally it may ask the server what privileges the user has (`has_table_privilege` on Postgres, `SHOW GRANTS` on MySQL) to grey out editing, but the server stays the authority.
- **No read-only mode in Rowlark.** For a connection that must not write, the user connects with a database user that only has `SELECT` (or whatever privileges they choose). The new-connection dialog may say so. This keeps one source of truth: the server's privileges.
- **One app-side guard: a Confirm | Cancel prompt** before DROP, TRUNCATE and DELETE/UPDATE without WHERE. It is a prompt only. It never blocks, and the server still decides whether the statement may run.
- **Edits and SQL generation:** edits go by primary key with a SQL preview; values are sent with an explicit cast and never built by pasting strings into SQL (use parameters or casts).
- **No environment color tags** (no prod/staging/local dots or colors).
- **Local data:** saved connections and query history live in SQLite. Add a "don't record history" option and a retention limit; history may contain sensitive literals. Exports are written only where the user chooses.
- **Supply chain:** CI runs `cargo audit`; commit lockfiles; add dependencies only with a reason (pinned UI framework versions especially); pin the Rust toolchain.
- **Release (Phase 5):** code signing and notarization; signed auto-updates (updater keys kept offline); publish checksums.
- **Reporting:** add a `SECURITY.md` with a private reporting contact before inviting outside use.

Known gap: query history encryption at rest is undecided (see open questions).
