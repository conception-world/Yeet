---
layout: home

hero:
  name: "Yeet"
  text: "Roblox Studio ↔ IDE sync"
  tagline: Bidirectional file sync between Roblox Studio and your editor (VS Code, Cursor, Antigravity). Edit anywhere, see it everywhere within ~200 ms — no restarts, no manual pushes.
  image:
    src: /icon.png
    alt: Yeet
  actions:
    - theme: brand
      text: Get started
      link: /getting-started
    - theme: alt
      text: VS Code Marketplace
      link: https://marketplace.visualstudio.com/items?itemName=ConceptionWorld.yeet
    - theme: alt
      text: Roblox Creator Store
      link: https://create.roblox.com/store/asset/126422641897714/Yeet
    - theme: alt
      text: GitHub
      link: https://github.com/conception-world/Yeet

features:
  - title: True bidirectional sync
    icon: 🔄
    details: Edit a script in Studio or on disk — both sides converge within ~200 ms. Echo detection prevents ping-pong loops.
  - title: 3-pane conflict merge
    icon: 🔀
    details: When both sides change the same file, you get a side-by-side line-by-line picker inside Studio. No silent overwrites, no lost work.
  - title: Rojo-compatible
    icon: 📐
    details: Uses the standard `default.project.json` layout. Existing Rojo projects work out of the box — Yeet is not a fork.
  - title: Reverse bootstrap
    icon: ⬇️
    details: Open an existing Studio project in VS Code with one click. Yeet reads the DataModel, scaffolds the file tree, and you keep editing.
  - title: Works in any VS Code fork
    icon: 🧩
    details: Tested in VS Code, Cursor, and Antigravity. The extension uses only stable VS Code APIs.
  - title: Local-only by design
    icon: 🔒
    details: WebSocket on `127.0.0.1:34872` with Origin allowlist + auth token. No telemetry, no cloud relay, no remote access.
---

## What is Yeet?

Yeet keeps your Roblox Studio scripts and your on-disk Luau files in
sync, both ways, in real time. You edit in your favourite IDE — full
LSP, version control, AI assistance — and the changes show up in
Studio. You edit in Studio while testing in-game, and the changes hit
disk. Conflicts show up as a 3-pane merge picker, not as silent data
loss.

It's three components:

- **Daemon** — a local Rust process that watches the project root
  and arbitrates between Studio and disk.
- **Plugin** — a Luau plugin that runs inside Studio and talks to
  the daemon.
- **Extension** — a TypeScript VS Code extension that manages the
  daemon's lifecycle and adds IDE-side commands.

All three speak JSON over a local WebSocket. Nothing leaves your
machine.

## 30-second quickstart

1. Install the extension from the
   [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=ConceptionWorld.yeet)
2. Install the plugin from the
   [Roblox Creator Store](https://create.roblox.com/store/asset/126422641897714/Yeet)
3. In your Roblox project folder:

In VS Code:

1. **`Yeet: Start`** → daemon launches; status bar shows `Yeet: running`.
2. Open the same place in Studio → click **Connect** in the Yeet plugin dock.
3. Edit a script anywhere. Save. It appears on the other side within ~200 ms.

[Full installation guide →](/getting-started)
