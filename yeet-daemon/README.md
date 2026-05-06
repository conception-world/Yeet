# yeet-daemon

The orchestrator process. Rust, async Tokio, WebSocket server on `127.0.0.1:34872`.

In Phase 0 it's a JSON echo: receives `{"type":"hello","version":"..."}` over a WebSocket and replies with `{"type":"ack","ack_of":"hello"}`. No tree state, no filesystem watching — that arrives in Phase 1.

## Build

```
cargo build --release
```

Release binary lands at `target/release/yeet-daemon(.exe)`.

## Run

```
cargo run
# or for a verbose run
RUST_LOG=yeet_daemon=debug cargo run
```

The daemon binds `127.0.0.1:34872` and logs `yeet-daemon listening`. Ctrl+C for clean shutdown.

## Smoke test

With the daemon running, in another terminal:

```
npx wscat -c ws://127.0.0.1:34872
> {"type":"hello","version":"0.0.1"}
< {"type":"ack","ack_of":"hello"}
```

The daemon logs the incoming message and the ack it sent back.

## Wire format

Phase 0 uses JSON (text frames). Phase 1 switches the main data path to MessagePack (binary frames); the `rmp-serde` dependency is already declared so that migration only needs the wiring.

## Lints

```
cargo clippy --all-targets -- -D warnings
```

The crate denies `clippy::all` and warns on `clippy::pedantic`. `unsafe_code` is forbidden.
