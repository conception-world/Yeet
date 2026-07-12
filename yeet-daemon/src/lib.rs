//! Library surface for the Yeet daemon. The binary (`src/main.rs`) is a thin
//! wrapper that wires up tokio + the WebSocket listener; all stateful logic
//! lives here so integration tests under `tests/` can exercise it directly.

// This is an internal crate, never published. The pedantic lints below trip
// on every `pub` item in a way that would bury the signal-to-noise ratio of
// the warning list without improving the code.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::implicit_hasher,
)]

pub mod audit;
pub mod auth;
pub mod merge;
pub mod project;
pub mod protocol;
pub mod sourcemap;
pub mod state;
pub mod syncback;
pub mod tree;
pub mod watcher;
