# Installation

Yeet has two pieces you install separately: a **VS Code extension**
(via Marketplace) and a **Roblox Studio plugin** (via the Creator
Store or by drag-and-dropping the `.rbxm` into your Plugins folder).
The Rust daemon ships **bundled inside the extension** for Windows
x64 — you don't install it separately on Windows.

## Requirements

- **VS Code 1.90+**, Cursor, or Antigravity
- **Roblox Studio** with HTTP requests enabled (see below)
- **Windows x64** for the bundled daemon — macOS / Linux users build
  from source or download from
  [GitHub Releases](https://github.com/conception-world/Yeet/releases).
  See [Daemon binary on macOS / Linux](#daemon-binary-on-macos-linux)
  below.

### Enable HTTP Requests in Studio

The plugin uses Studio's `HttpService` to talk to the local daemon.
Once per place file:

1. Open the place in Studio
2. **File → Game Settings → Security**
3. Toggle **Allow HTTP Requests** to on
4. **Save**

This is a Studio-level requirement; without it the plugin fails to
connect with a clear error in the dock log.

## 1. Install the VS Code extension

Open the
[Yeet on the VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=ConceptionWorld.yeet)
and click **Install**. Or, from inside VS Code: open the Extensions
panel (`Ctrl+Shift+X`), search for **Yeet**, and install the one
published by `Conception World`. Or via command line:

```bash
code --install-extension ConceptionWorld.yeet
```

The extension activates on workspaces that contain a
`default.project.json` (Rojo project marker). If your project doesn't
have one yet, run **Yeet: Create** from the command palette
(`Ctrl+Shift+P`) — it scaffolds the standard Rojo layout plus `src/`
directories for the common service mounts.

## 2. Install the Studio plugin

### Option A — Roblox Creator Store (recommended)

1. Open the
   [Yeet plugin page on the Roblox Creator Store](https://create.roblox.com/store/asset/126422641897714/Yeet)
2. Click **Install**
3. Reopen Studio (or click the plugin button in the Plugins toolbar)

### Option B — Manual drop-in

1. Download `Yeet.rbxm` from the
   [GitHub Releases page](https://github.com/conception-world/Yeet/releases)
2. Drop it into your Studio Plugins folder:
   - **Windows**: `%LOCALAPPDATA%\Roblox\Plugins\`
   - **macOS**: `~/Documents/Roblox/Plugins/`
3. Reopen Studio

You should now see a **Yeet** button in the Plugins toolbar.

## 3. First sync

In VS Code:

1. Open your Roblox project folder.
2. **`Yeet: Start`** from the command palette. The status bar shows
   `Yeet: running (:34872)` once the daemon is up — the number is the
   port this project's daemon took.
3. Open the same place in Studio.
4. Click the **Yeet** toolbar button to open the plugin dock.
5. Under **Project**, pick your project from the list. The plugin
   scans for running daemons and shows one row per project it finds,
   with its root path and port. With a single project open there will
   be exactly one row.
6. Click **Connect**. You should see "Connected" within a second —
   no manual pairing required (the extension keeps a fresh pairing
   breadcrumb at `<root>/.yeet/pairing` while the daemon runs).
7. Edit a script in either side and save. It appears on the other
   within ~200 ms.

Studio remembers which project you picked, per place, so you only do
step 5 once per place file.

If something doesn't work, check
[Troubleshooting](/troubleshooting) — most issues are HTTP-requests
being off, the project not appearing in the picker (the daemon isn't
running, or it landed outside the scanned port window — see below), or
SmartScreen blocking the bundled daemon's first run.

## Working on more than one project

Each project root gets its own daemon, so you can sync two projects at
the same time:

1. Open project A in one IDE window and project B in another. Run
   **`Yeet: Start`** in each.
2. The first daemon takes port `34872`, the second takes `34873`, and
   so on — the daemon walks the window `34872..34881` and takes the
   first free port. Each window's status bar shows its own port.
3. Open both places in Studio.
4. In each place's Yeet panel, pick that place's project from the
   **Project** list, then **Connect**.

Rows in the picker carry an **"in use"** badge when that daemon
already has a plugin connected to it — usually another open place. Use
it as a signal that you're about to take a connection away from that
place, not as an error. **Refresh** re-runs the scan if you started a
daemon after opening the panel.

Two caveats:

- The picker only sees daemons inside the `34872..34881` window. If
  all ten ports are taken, a further daemon still starts and still
  syncs, but on an OS-assigned port outside the window — it logs a
  warning saying so, and the plugin's scan won't list it. Connect it
  by setting the plugin's Daemon URL to the address the daemon logged.
- Confirm you picked the right row before connecting. The root path is
  shown precisely so two projects with the same name can be told
  apart; pointing a place at the wrong project's daemon would sync the
  wrong file tree into it.

## Daemon binary on macOS / Linux

The bundled daemon is **Windows x64 only** in v0.3.0. macOS and Linux
users have two options:

### Build from source

```bash
git clone https://github.com/conception-world/Yeet
cd Yeet/yeet-daemon
cargo build --release
# binary lands at: yeet-daemon/target/release/yeet-daemon
```

Then in VS Code Settings, set:

```json
"yeet.daemonPath": "/absolute/path/to/yeet-daemon/target/release/yeet-daemon"
```

### Download a pre-built binary

Open the
[GitHub Releases page](https://github.com/conception-world/Yeet/releases)
and download the binary for your platform from the latest release
(if attached). Set `yeet.daemonPath` to its absolute path.

Cross-platform bundling (macOS arm64/x64, Linux x64) is on the
v0.3.1 roadmap.

## Windows: SmartScreen on first run

The bundled `yeet-daemon.exe` is **not yet code-signed**, so Windows
SmartScreen may block the first launch with "Windows protected your
PC". Click **More info → Run anyway** if you trust the release
source. This only happens once; Windows remembers the decision.

Code signing is on the v0.3.1 roadmap.

## Next steps

- [Daily workflow](/workflow) — how to actually use Yeet day-to-day
- [Settings reference](/settings) — every setting explained
- [How it works](/architecture) — the 3-component architecture
