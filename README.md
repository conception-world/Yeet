# Yeet

Bidirectional sync between Roblox Studio and IDEs (VS Code / Antigravity), with a three-pane conflict resolver and reverse-bootstrap from an existing place.

## Architecture

```
+----------------------+         +----------------------+         +----------------------+
|   Roblox Studio      |  WS     |    yeet-daemon       |  spawn  |   VS Code / Antig.   |
|   (yeet-plugin)      | <-----> |  (Rust, :34872..81)  | <-----  |   (yeet-extension)   |
|                      |         |                      |         |                      |
|   DockWidget UI      |         |   Tree_Studio        |         |   Status bar, cmd    |
|   Project picker     |         |   Tree_FS            |         |   yeet.start / stop  |
|   WebSocket client   |         |   Tree_Base (merge)  |         |                      |
+----------------------+         +----------------------+         +----------------------+
                                            |
                                            |  file I/O (Rojo-compatible project layout)
                                            v
                                   +----------------------+
                                   |   Filesystem (repo)  |
                                   +----------------------+
```

Three components, one repo:

- [`yeet-daemon/`](yeet-daemon/) — Rust CLI binary. The orchestrator. Holds the three in-memory trees and performs 3-way merges on conflict. WebSocket server bound to loopback on the first free port in `127.0.0.1:34872..34881`.
- [`yeet-plugin/`](yeet-plugin/) — Luau plugin for Roblox Studio. `DockWidgetPluginGui` for the conflict resolver, bootstrap UI and project picker. Connects to the daemon via native WebSocket.
- [`yeet-extension/`](yeet-extension/) — TypeScript extension for VS Code / Antigravity. Spawns and supervises the daemon.

Wire format: JSON during Fase 0 (scaffolding), switching to MessagePack in Fase 1.

## One daemon per project

Each project root gets its own daemon. The daemon walks the port window
`34872..34881` and takes the first free port, so the first project still lands on
`34872`, a second on `34873`, and so on. If every port in the window is busy, it
falls back to an OS-assigned ephemeral port — it still serves, but it warns that
the Studio plugin's scan will not find it.

Two things make that discoverable:

- **`~/.yeet/daemons.json`** — each daemon writes a row at startup (`daemon_id`,
  `pid`, `port`, `project_root`, `project_name`, `daemon_version`, `started_at`).
  The extension reads it to answer "is a daemon already serving this root?" and
  reuses that daemon instead of spawning a duplicate. Stale rows are pruned by
  probing the port (`pid` is recorded for humans, never used for liveness). The
  registry is advisory: if it is missing or corrupt, the extension just spawns
  its own daemon.
- **A discovery handshake** — the plugin has no filesystem and no HTTP client
  inside Studio, so it cannot read the registry. Instead it opens a short-lived
  WebSocket to each port in the window, sends `Hello { role: "discover" }`, and
  each daemon replies with `daemon_info` (`daemon_id`, `project_name`,
  `project_root`, `port`, `daemon_version`, `plugin_connected`) and closes. The
  daemon answers this before its version gate and its auth gate, under a read
  lock only, and never rotates the session id — scanning cannot disturb a plugin
  already connected to one of the scanned daemons, nor consume its pairing
  breadcrumb. The frame carries identity only: never the auth token, never file
  contents.

The Yeet panel in Studio renders the results as a **Project** list (name, root
path, port, and an "in use" badge when another place already holds that
daemon's session) plus a Refresh button. The choice is remembered per place, and
the plugin keys its stored auth token per project so two daemons no longer
invalidate each other's token.

On the IDE side, the extension learns the port from the daemon's stdout
(`yeet-port: <n>`, alongside the existing `yeet-auth-token: <hex>` line) and
shows it in the status bar as `Yeet: running (:34873)`. The daemon also logs the
address it bound.

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
