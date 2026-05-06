# Yeet — Roblox Studio ↔ IDE sync

Bidirectional file sync between **Roblox Studio** and your editor (VS
Code, Cursor, Antigravity, or any VS Code fork). Edit a script in
either side and it appears on the other within ~200 ms. Conflicts
are surfaced through a 3-pane line-by-line merge picker in Studio,
not silently overwritten.

Yeet uses a Rojo-compatible project layout (`default.project.json`)
so existing projects pick up sync without restructuring.

---

## Requirements

- **VS Code** 1.90+ (also works in Cursor and Antigravity).
- **Roblox Studio** with the Yeet plugin installed (download from
  the project's GitHub Releases page).
- **Allow HTTP Requests** enabled in Studio: File → Game Settings →
  Security → Allow HTTP Requests. The plugin's first connect
  attempt will surface this if it's off.

### Daemon binary

The daemon binary ships pre-built **only for Windows (x64)** in this
release. macOS / Linux users have two options:

1. **Download a pre-built binary** from the
   [GitHub Releases page](https://github.com/conception-world/Yeet/releases),
   then point `yeet.daemonPath` at it (Settings → search "yeet").
2. **Build from source** — clone the repo and run
   `cargo build --release` in `yeet-daemon/`. The binary lands at
   `yeet-daemon/target/release/yeet-daemon[.exe]`. Set
   `yeet.daemonPath` to that absolute path.

Cross-platform bundling (macOS arm64/x64, Linux x64) is on the
v1.1 roadmap.

### Windows: SmartScreen warning on first run

The bundled `yeet-daemon.exe` is **not yet code-signed**, so Windows
SmartScreen may block the first launch with a "Windows protected
your PC" prompt. Click **More info → Run anyway** if you trust the
release source. Code signing is on the v1.1 roadmap.

---

## Quick start

1. Install the extension from the VS Code Marketplace.
2. Install the Yeet plugin in Studio (drop `Yeet.rbxm` into your
   `Plugins/` folder).
3. Open your Roblox project folder in VS Code.
4. Run **Yeet: Create** if you don't already have a
   `default.project.json` — it scaffolds the standard Rojo layout
   plus `src/` directories for the common service mounts.
5. Run **Yeet: Start** — daemon launches, status bar shows
   `Yeet: running`.
6. Open the same place in Studio, click **Connect** in the Yeet
   plugin dock. Sync is live.

---

## Commands

| Command | What it does |
|---------|-------------|
| `Yeet: Start` | Spawns the bundled daemon for the current workspace. |
| `Yeet: Stop` | Gracefully terminates the daemon. |
| `Yeet: Create` | Scaffolds a new Rojo-compatible project at the workspace root. |
| `Yeet: Sync From Studio` | One-shot push: Studio's DataModel becomes the source of truth; disk files are recreated to match. |
| `Yeet: Sync From IDE` | Inverse: disk files overwrite Studio's DataModel. |
| `Yeet: Pair Studio` | Manual fallback to refresh the auto-pair breadcrumb if Studio missed it. |

---

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `yeet.daemonPath` | empty (use bundled) | Override path to a custom-built `yeet-daemon` binary. Scoped to user/machine — workspace `.vscode/settings.json` cannot override this for security reasons. |
| `yeet.debugEcho` | `false` | Pass `--debug-echo` to the daemon so every hash and echo-detection decision is logged. Useful when diagnosing ping-pong loops between Studio and disk. |
| `yeet.createTemplate` | `["ServerScriptService", ...]` | Service scopes that `Yeet: Create` scaffolds into `src/`. Empty array → no `src/` folders. |

---

## Troubleshooting

**Plugin shows "Disconnected" and clicking Connect does nothing.**
Check that `Yeet: Start` is running (status bar shows the daemon).
If yes, verify Studio's HTTP requests are enabled (see Requirements).

**The dock log says `HTTP requests are disabled in this place`.**
File → Game Settings → Security → Allow HTTP Requests = on. This
setting is per-place, not per-Studio.

**`yeet.daemonPath does not exist` error on Yeet: Start.**
Either set `yeet.daemonPath` to a valid binary path, or clear the
setting so the extension uses the bundled daemon (recommended).

**Plugin connects but bulk sync interface doesn't appear.**
Reload the plugin (right-click in Roblox Studio → Plugins folder →
Reinstall) — sometimes Studio's plugin layout cache holds an old
WIDGET_ID. As a fallback, use the toolbar's "Show Sync" button.

**Sync seems to ping-pong: edits on one side instantly become edits on the other.**
This is the echo-detection cache; it should self-correct in <1s.
If it persists, enable `yeet.debugEcho` and inspect the Output
channel for `isEcho` decisions.

**Studio is slow / freezes during a 1000+ script bootstrap.**
The plugin renders the bulk-sync preview progressively (100 rows
per frame), but the underlying snapshot can still take ~5–10 s on
very large projects. Wait for the dock to populate.

---

## How it works

1. **Daemon** (Rust) watches the project's `default.project.json`
   `$path` mappings and mirrors disk files into a tree.
2. **Plugin** (Luau) walks the DataModel and reports the Studio
   side. Daemon diffs the two trees; conflicts and divergences are
   shown in a Roact-based dock.
3. **Extension** (TypeScript) manages the daemon's lifecycle and
   handles "open this folder" requests from the daemon (with a
   confirmation prompt).

All three components communicate via JSON over a local-only
WebSocket on `127.0.0.1:34872`. Browsers and remote clients are
rejected at the upgrade via Origin allowlist.

---

## License

MIT — see [LICENSE](./LICENSE).

## Issues / contributions

Bugs, feature requests: <https://github.com/conception-world/Yeet/issues>.
