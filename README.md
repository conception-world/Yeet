# Yeet

Bidirectional sync between Roblox Studio and IDEs (VS Code / Antigravity), with a three-pane conflict resolver and reverse-bootstrap from an existing place.

## Architecture

```
+----------------------+         +----------------------+         +----------------------+
|   Roblox Studio      |  WS     |    yeet-daemon       |  spawn  |   VS Code / Antig.   |
|   (yeet-plugin)      | <-----> |    (Rust, :34872)    | <-----  |   (yeet-extension)   |
|                      |         |                      |         |                      |
|   DockWidget UI      |         |   Tree_Studio        |         |   Status bar, cmd    |
|   WebSocket client   |         |   Tree_FS            |         |   yeet.start / stop  |
|                      |         |   Tree_Base (merge)  |         |                      |
+----------------------+         +----------------------+         +----------------------+
                                            |
                                            |  file I/O (Rojo-compatible project layout)
                                            v
                                   +----------------------+
                                   |   Filesystem (repo)  |
                                   +----------------------+
```

Three components, one repo:

- [`yeet-daemon/`](yeet-daemon/) — Rust CLI binary. The orchestrator. Holds the three in-memory trees and performs 3-way merges on conflict. WebSocket server on `127.0.0.1:34872`.
- [`yeet-plugin/`](yeet-plugin/) — Luau plugin for Roblox Studio. `DockWidgetPluginGui` for the conflict resolver and bootstrap UI. Connects to the daemon via native WebSocket.
- [`yeet-extension/`](yeet-extension/) — TypeScript extension for VS Code / Antigravity. Spawns and supervises the daemon.

Wire format: JSON during Fase 0 (scaffolding), switching to MessagePack in Fase 1.

## Phase 0 status

Current phase: **0 — Scaffolding**. Goal: the three components compile and trade a `hello`/`ack` round-trip over WebSocket. No tree synchronization yet.

## Prerequisites

- Rust stable toolchain (edition 2024 — Rust ≥ 1.85).
- Node 20+ and pnpm.
- [Rojo](https://rojo.space) 7+ on `PATH`.
- Roblox Studio.

## Setup

```
cd yeet-daemon    && cargo build --release
cd yeet-extension && pnpm install && pnpm build
cd yeet-plugin    && rojo build --output build/Yeet.rbxm
```

To push the plugin into Studio while developing Yeet itself (Yeet cannot sync itself yet — chicken and egg):

```
./scripts/sync-plugin.ps1
```

See each subproject's README for component-specific details.

## Layout

```
Yeet/
  yeet-daemon/       Rust orchestrator
  yeet-plugin/       Luau plugin, built with Rojo
  yeet-extension/    TypeScript VS Code extension
  scripts/           Dev automation (plugin sync, etc.)
```
