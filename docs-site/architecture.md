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
         │ ws://127.0.0.1:34872                     │ ws://127.0.0.1:34872
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
- Accepts WebSocket connections on `127.0.0.1:34872`. Origin
  allowlist + auth token gate the upgrade — browsers and remote
  clients are rejected before any frame is read.
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
- Renders the dock UI: connection status, Activity log, BulkSync
  preview, ConflictResolver 3-pane merge.
- Persists user-facing settings via `plugin:SetSetting` (with a
  JSON wrapper because raw booleans are unreliable across some
  Studio builds — see [troubleshooting](/troubleshooting#settings-dock-toggle-flips-back-to-off)).

### Extension (TypeScript)

Lives in [`yeet-extension/`](https://github.com/conception-world/Yeet/tree/main/yeet-extension).
Strict TypeScript, esbuild bundle, no runtime deps beyond `ws`.
Responsibilities:

- Spawns and supervises the daemon process via `child_process.spawn`.
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
| `Hello { version, role }` | client → daemon | First frame. Role distinguishes plugin from extension. |
| `Welcome { daemon_version, project_root }` | daemon → plugin | Confirms handshake, names the project the daemon is serving. |
| `FileSnapshot[]` | plugin → daemon | Initial DataModel state on plugin Hello. |
| `FileChanged { path, source }` | bidirectional | Mirrored edit. |
| `OpenProjectRequest { path }` | daemon → extension | Asks the IDE to open a folder (always gated by user modal). |
| `BulkSyncFromStudioRequest` / `BulkSyncFromIdeRequest` | extension → daemon | Triggers the one-shot bulk migration flow. |

Frame size is capped at 16 MiB to bound buffering under attacker
flood scenarios.

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

Yeet's network surface is a single TCP listener on
`127.0.0.1:34872`. To prevent abuse:

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

## Source code

- Daemon: [`yeet-daemon/src/`](https://github.com/conception-world/Yeet/tree/main/yeet-daemon/src)
- Plugin: [`yeet-plugin/src/`](https://github.com/conception-world/Yeet/tree/main/yeet-plugin/src)
- Extension: [`yeet-extension/src/`](https://github.com/conception-world/Yeet/tree/main/yeet-extension/src)

The daemon ships with 131 passing tests covering the core sync
logic, conflict resolution, and protocol compatibility. Run with
`cargo test --release` from `yeet-daemon/`.
