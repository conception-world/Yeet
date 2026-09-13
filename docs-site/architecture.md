# How it works

Yeet has three components that run on the same machine and talk to
each other over a local WebSocket. Understanding what each does is
useful when debugging weird states or contributing.

## The three components

```
┌──────────────────┐                       ┌──────────────────┐
│  Roblox Studio   │                       │   VS Code / IDE  │
│                  │                       │                  │
│  ┌────────────┐  │                       │  ┌────────────┐  │
│  │   Plugin   │  │                       │  │ Extension  │  │
│  │   (Luau)   │  │                       │  │    (TS)    │  │
│  └─────┬──────┘  │                       │  └─────┬──────┘  │
└────────┼─────────┘                       └────────┼─────────┘
         │ WebSocket                                │ WebSocket
         │ ws://127.0.0.1:PORT                      │ ws://127.0.0.1:PORT
         │ (first free in 34872..34881)             │
         │                                          │
         │           ┌──────────────────┐           │
         └──────────►│      Daemon      │◄──────────┘
                     │      (Rust)      │
                     │                  │
                     │   project root   │
                     │   on disk ←──────┼──── reads/writes .luau files
                     └──────────────────┘
```

### Daemon (Rust)

Lives in [`yeet-daemon/`](https://github.com/conception-world/Yeet/tree/main/yeet-daemon).
Built on `tokio` + `tungstenite` + `notify`. Responsibilities:

- Watches the project's `default.project.json` `$path` mappings and
  builds an in-memory tree of disk files.
- Accepts WebSocket connections on loopback, on the first free port
  in the window `34872..34881` (see
  [One daemon per project](#one-daemon-per-project)). Origin
  allowlist + auth token gate the upgrade — browsers and remote
  clients are rejected before any frame is read.
- Registers itself in `~/.yeet/daemons.json` at startup and removes
  its row on clean shutdown.
- Answers discovery probes so the Studio plugin can find it.
- Routes frames between the plugin and the extension.
- Emits sync deltas (file changed, file added, file removed) to
  whoever asks.
- Diffs the plugin's snapshot against the disk tree on connect to
  drive bulk-sync previews and conflict detection.

The daemon is the only component that touches disk. Both the plugin
and extension are FS-free.

### Plugin (Luau)

Lives in [`yeet-plugin/`](https://github.com/conception-world/Yeet/tree/main/yeet-plugin).
Strict Luau (`--!strict`), uses Roact for the dock UI. Responsibilities:

- Walks the DataModel and reports it as a `FileSnapshot` array on
  Hello.
- Subscribes to `script.Source` changes via `getPropertyChangedSignal`
  and forwards edits to the daemon.
- Receives daemon-pushed edits and applies them to the matching
  script in Studio (preserving undo history).
- Scans the port window for running daemons and renders the result
  as the **Project** picker (see
  [One daemon per project](#one-daemon-per-project)).
- Renders the dock UI: connection status, Activity log, BulkSync
  preview, ConflictResolver 3-pane merge.
- Persists user-facing settings via `plugin:SetSetting` (with a
  JSON wrapper because raw booleans are unreliable across some
  Studio builds — see [troubleshooting](/troubleshooting#settings-dock-toggle-flips-back-to-off)).

### Extension (TypeScript)

Lives in [`yeet-extension/`](https://github.com/conception-world/Yeet/tree/main/yeet-extension).
Strict TypeScript, esbuild bundle, no runtime deps beyond `ws`.
Responsibilities:

- Spawns and supervises the daemon process via `child_process.spawn`,
  and learns the port it bound from the daemon's `yeet-port: <n>`
  stdout line (printed next to the existing `yeet-auth-token: <hex>`).
- Reads `~/.yeet/daemons.json` before spawning: if a live daemon is
  already serving this project root, it attaches to that one instead
  of starting a duplicate.
- Bundles the daemon binary at `bin/win-x64/yeet-daemon.exe` so the
  user doesn't have a separate install step on Windows.
- Handles `open_project_request` frames from the daemon (with a
  mandatory user-confirmation modal — see
  [security note below](#security-model)).
- Manages the auto-pair breadcrumb at `<root>/.yeet/pairing` so the
  Studio plugin connects on first click.
- Surfaces commands (`Yeet: Start`, `Yeet: Stop`, etc.) and the
  status bar item.

The extension is **stateful only about the daemon process**; everything
sync-related is asked of the daemon.

## Wire protocol

JSON over WebSocket. Every frame has a `type` discriminator. The
daemon's [`protocol.rs`](https://github.com/conception-world/Yeet/blob/main/yeet-daemon/src/protocol.rs)
is the source of truth; plugin and extension keep narrow type views
of the variants they consume.

Key frames:

| Frame | Direction | Purpose |
|---|---|---|
| `Hello { version, role }` | client → daemon | First frame. Role distinguishes plugin from extension. `role: "discover"` makes it a discovery probe. |
| `DaemonInfo { daemon_id, project_name, project_root, port, daemon_version, plugin_connected }` | daemon → plugin | Reply to a discovery probe. The daemon closes the socket right after. |
| `Welcome { daemon_version, project_root }` | daemon → plugin | Confirms handshake, names the project the daemon is serving. |
| `FileSnapshot[]` | plugin → daemon | Initial DataModel state on plugin Hello. |
| `FileChanged { path, source }` | bidirectional | Mirrored edit. |
| `OpenProjectRequest { path }` | daemon → extension | Asks the IDE to open a folder (always gated by user modal). |
| `BulkSyncFromStudioRequest` / `BulkSyncFromIdeRequest` | extension → daemon | Triggers the one-shot bulk migration flow. |

Frame size is capped at 16 MiB to bound buffering under attacker
flood scenarios.

## One daemon per project

There is one daemon per project root, not one daemon per machine.

### The port window

The daemon binds loopback on the **first free port in `34872..34881`**.
The first project you open still lands on `34872`, so nothing changes
if you only ever work on one project; a second project lands on
`34873`, and so on. If every port in the window is taken, the daemon
falls back to an OS-assigned ephemeral port — it still starts and
still syncs, but it logs a warning that the Studio plugin's scan will
not find it, since the scan only covers the window.

To see which port a daemon took: the IDE status bar reads
`Yeet: running (:34873)`, and the daemon logs the address it bound at
startup.

### The registry (`~/.yeet/daemons.json`)

Each daemon writes a row there at startup — `daemon_id`, `pid`,
`port`, `project_root`, `project_name`, `daemon_version`,
`started_at` — and removes it on clean shutdown.

The **extension** reads it to answer one question: "is a daemon
already serving this project root?" If so, it attaches to that daemon
instead of spawning a duplicate. Rows left behind by a crash are
pruned by probing the port; the recorded `pid` is for humans reading
the file and is never used as a liveness check.

The registry is advisory. If it is missing, unreadable, or corrupt,
nothing breaks — the extension just spawns its own daemon.

### Discovery, and the project picker

The plugin cannot read the registry: inside Studio there is no
filesystem access and no HTTP client, only
`HttpService:CreateWebStreamClient`. So it finds daemons the only way
it can — it opens a short-lived WebSocket to each port in the window,
sends `Hello { role: "discover" }`, and each daemon that answers
replies with a `DaemonInfo` frame and closes the connection.

The daemon answers a discovery probe **before its version gate and
before its auth gate**, takes only a read lock, and never rotates the
session id. That matters: scanning must not disturb a plugin already
connected to one of the scanned daemons, and must not consume anyone's
one-shot pairing breadcrumb. Answering before the version gate also
means a plugin that is too old or too new for a given daemon can still
*see* it and say so, instead of silently missing it.

The Yeet dock renders the results as a **Project** list: one row per
daemon found, showing the project name, its root path and its port,
plus a Refresh button. You click the project this place belongs to,
and the choice is remembered per place. A row carries an **"in use"**
badge when that daemon already has a plugin session — it is another
open place talking to it, and connecting there would take the
connection away from it.

The plugin stores its auth token **per project**, keyed by
`daemon_id`. A single global key used to make two daemons clear each
other's token in a reject/re-pair loop.

### Working on two projects at once

Open project A in one IDE window and project B in another, and run
`Yeet: Start` in each; you get two daemons on two ports. Open both
places in Studio, and in each place's Yeet panel pick the project that
place belongs to. Each place then syncs only against its own project's
daemon.

## Conflict resolution

When the daemon sees a `FileChanged` on disk and a `FileChanged` from
the plugin for the same file between two sync ticks, it doesn't
auto-merge. Instead, it computes a **3-way diff** against the last
synced version and emits a `ConflictDetected` frame to the plugin.

The plugin's `ConflictResolver` dock renders this as a 3-pane merge
(IDE / common ancestor / Studio) and lets the user pick lines or
hunks. The result is sent back to the daemon as `ConflictResolved`,
which writes both sides.

There's no auto-resolve heuristic — silent merging is how lost work
happens, and the cost of asking the user explicitly is bounded
(conflicts are rare in normal use).

## Security model

Yeet's network surface is a single loopback TCP listener per daemon,
on a port in the `34872..34881` window. To prevent abuse:

- **Loopback bind only**: external clients can't reach the daemon
  at all.
- **Origin allowlist on upgrade**: browsers (which always send an
  `Origin` header) are rejected. Native clients (Studio's
  `WebStreamClient`, the extension's `ws`) are accepted.
- **Auth token**: 256 bits of `OsRng`, written to
  `<root>/.yeet/auth-token` (perm 0600 on Unix). Only callers with
  local FS read access can recover it. Optional but validated when
  present.
- **Concurrent connection cap**: 4 simultaneous connections max,
  bounding peak memory under DoS.
- **Frame-size cap**: 16 MiB, to prevent unbounded buffering.

The **discovery endpoint is unauthenticated by design** — the plugin
has to be able to ask "who is there?" before it can know which
project's token to present, and running the probe through the auth
gate would burn the one-shot pairing breadcrumb of projects the user
never meant to connect to. It is therefore readable by any local
process that can open a loopback socket, so it returns only
non-sensitive identity: `daemon_id`, `project_name`, `project_root`,
`port`, `daemon_version`, `plugin_connected`. Never the auth token,
never file contents, never a session id. `project_root` is the one
arguably sensitive field, and it is already disclosed to any
unauthenticated plugin via `ProjectOpened.project_root` — the picker
needs it to tell apart two projects that share a name. Everything
past the probe still goes through the Origin allowlist and the auth
token.

The `open_project_request` flow has an additional defence: the
extension always shows a modal naming the path and requires
explicit user approval before calling `vscode.openFolder`. Without
it, a hijacked daemon (or a malicious local process spoofing the
daemon) could trick the user into opening a hostile folder, which
is RCE on VS Code (`.vscode/tasks.json` with `"runOn": "folderOpen"`
runs shell commands).

## Why three components

Could the plugin talk directly to the extension? Technically yes,
but the daemon earns its keep:

- **Studio is single-threaded and slow at FS work.** Pushing the
  filesystem watcher into Rust gives sub-100 ms edit latency.
- **VS Code extensions can't easily own a long-running socket.**
  The extension's `child_process.spawn` model lets the daemon
  outlive any single VS Code window.
- **Conflict resolution is intricate.** Doing the 3-way merge in
  Rust means we can ship the same logic between IDEs without
  re-implementing it per host.
- **Multiple plugins can connect to one daemon.** Studio + a
  hypothetical second IDE talking to the same project tree just
  works.
- **And multiple daemons can coexist.** One per project root, each on
  its own port, each discoverable — so two projects open side by side
  don't fight over a single process or a single port. A per-machine
  singleton could do neither.

## Source code

- Daemon: [`yeet-daemon/src/`](https://github.com/conception-world/Yeet/tree/main/yeet-daemon/src)
- Plugin: [`yeet-plugin/src/`](https://github.com/conception-world/Yeet/tree/main/yeet-plugin/src)
- Extension: [`yeet-extension/src/`](https://github.com/conception-world/Yeet/tree/main/yeet-extension/src)

The daemon ships with 131 passing tests covering the core sync
logic, conflict resolution, and protocol compatibility. Run with
`cargo test --release` from `yeet-daemon/`.
