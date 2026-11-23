# Rowlark

A fast, small, open-source desktop database client for **PostgreSQL** and **MySQL/MariaDB**.

> **Status: early development.** The project scaffold is in place, but there is no database functionality yet. Nothing here is ready to use.

## Goals

- Small and quick: a ~10–15 MB app that starts in under a second (Tauri, not Electron).
- Keyboard-first and always dark mode.
- Exact types: decimals, big integers, JSON and timestamps are never rounded or reformatted on the way to the screen.
- Safe by default: confirmation for destructive statements, per-connection read-only mode, passwords only in the OS keychain.

See [PLAN.md](PLAN.md) for the full plan, architecture and roadmap.

## Platforms

macOS is the current focus. Windows and Linux are planned for later.

## Tech stack

- **Shell and backend:** [Tauri 2](https://tauri.app) (Rust)
- **UI:** React, TypeScript, Vite
- **Layout:** a Rust workspace with `crates/core` (drivers and types, no UI dependencies) and `src-tauri` (thin Tauri wrapper)

## Development

### Prerequisites

- [Node.js](https://nodejs.org) and npm
- [Rust](https://rustup.rs) via `rustup`. The exact version is pinned in `rust-toolchain.toml` and installs automatically.
- Xcode Command Line Tools (macOS): `xcode-select --install`
- A Docker-compatible runtime (Docker Desktop or Colima), only needed for the database tests

### Run

```sh
npm install
npm run tauri dev
```

### Checks

```sh
npm run typecheck                                 # TypeScript
npm run lint                                      # ESLint
cargo fmt --all --check                           # Rust formatting
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                            # Rust tests
```

### Test databases

`docker-compose.yml` starts PostgreSQL 14–17, MySQL 5.7/8/9 and MariaDB 11 for driver tests. Each server has its own host port. Credentials are dev-only: user, password and database are `rowlark` / `rowlark` / `rowlark_test`.

```sh
docker compose up -d              # all servers
docker compose up -d postgres16   # just one
docker compose down
```

MySQL 5.7 has no native arm64 image, so it runs under emulation on Apple Silicon.

## Contributing

Not open for contributions yet while the foundations are being built.

## License

Not yet decided. Until a license is added, all rights are reserved.
