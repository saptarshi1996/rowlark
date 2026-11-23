//! Rowlark core library.
//!
//! This crate must stay free of Tauri and UI dependencies so it can be tested
//! on its own. The `Value` enum and the `Driver` trait will live here (Phase 1).

pub mod value;

pub use value::Value;

/// Returns the version of the core crate, taken from Cargo.toml at compile time.
/// A placeholder that proves the workspace wiring works end to end.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!version().is_empty());
    }
}
