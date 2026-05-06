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

Open the Extensions panel (`Ctrl+Shift+X`) and search for **Yeet**.
Install the one published by `conception-world`. Or install via
command line:

```bash
code --install-extension conception-world.yeet
```

The extension activates on workspaces that contain a
`default.project.json` (Rojo project marker). If your project doesn't
have one yet, run **Yeet: Create** from the command palette
(`Ctrl+Shift+P`) — it scaffolds the standard Rojo layout plus `src/`
directories for the common service mounts.

## 2. Install the Studio plugin

### Option A — Roblox Creator Store (recommended)

1. Open [the Yeet plugin page on Roblox](https://create.roblox.com/store)
   (search for "Yeet" by `conception-world`)
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
   `Yeet: running` once the daemon is up.
3. Open the same place in Studio.
4. Click the **Yeet** toolbar button to open the plugin dock.
5. Click **Connect**. You should see "Connected" within a second —
   no manual pairing required (the extension keeps a fresh pairing
   breadcrumb at `<root>/.yeet/pairing` while the daemon runs).
6. Edit a script in either side and save. It appears on the other
   within ~200 ms.

If something doesn't work, check
[Troubleshooting](/troubleshooting) — most issues are HTTP-requests
being off, port 34872 being held by another process, or SmartScreen
blocking the bundled daemon's first run.

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
