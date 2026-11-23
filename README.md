# Rowlark

A fast, small desktop database client that works and looks good. It starts with **PostgreSQL** and **MySQL/MariaDB**, with more engines (including NoSQL) planned.

> **Status: early development.** Only the core type system exists so far. There is no UI and no database functionality yet. Nothing here is ready to use.

## Goals

- Fast and small: 100% Rust with a native, GPU-drawn UI. No webview, no Electron, no JavaScript.
- Keyboard-first and always dark mode.
- Exact types: decimals, big integers, JSON and timestamps are never rounded or reformatted on the way to the screen.
- Safe by default: a confirmation prompt for destructive statements, passwords only in the OS keychain. The database itself decides what is allowed.

See [PLAN.md](PLAN.md) for the full plan, architecture and roadmap.

## Platforms

macOS (Apple Silicon) is the current focus. Windows and Linux are planned for later.

## Tech stack

- **Language:** Rust, end to end
- **UI:** [GPUI](https://www.gpui.rs) (planned; confirmed by a small spike before screens are built)
- **Layout:** a Rust workspace. `crates/core` holds drivers and types with no UI dependencies. The UI crate is added after the first drivers work.

## Development

### Prerequisites

- [Rust](https://rustup.rs) via `rustup`. The exact version is pinned in `rust-toolchain.toml` and installs automatically.
- Xcode Command Line Tools (macOS): `xcode-select --install`
- A Docker-compatible runtime (Docker Desktop or Colima), only needed for the database tests

### Checks

```sh
cargo fmt --all --check                           # Rust formatting
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                            # Rust tests
```

### Test databases

`docker-compose.yml` starts PostgreSQL 14–17, MySQL 8/9 and MariaDB 11 for driver tests. Each server has its own host port. Credentials are dev-only: user, password and database are `rowlark` / `rowlark` / `rowlark_test`.

```sh
docker compose up -d              # all servers (Colima users: if `docker compose` is not found,
                                  # link the compose plugin into ~/.docker/cli-plugins or use `docker-compose`)
docker compose up -d postgres16   # just one
docker compose down
```

MySQL 5.7 is not included for now: it has no native arm64 image.

## Contributing

Not open for contributions yet while the foundations are being built.

## License

Not yet decided. Until a license is added, all rights are reserved.
