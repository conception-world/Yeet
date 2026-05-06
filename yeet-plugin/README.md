# yeet-plugin

Roblox Studio plugin (Luau, strict mode). Built with Rojo.

In Phase 0 the plugin surfaces a single `DockWidgetPluginGui` with a **Connect** button and a log panel. Clicking Connect opens a WebSocket to the daemon on `ws://127.0.0.1:34872`, sends `{"type":"hello","version":"0.0.1"}`, and logs whatever comes back.

## Prerequisites

- [Rojo](https://rojo.space) 7+ on `PATH`.
- Roblox Studio.

## Build

```
rojo build --output build/Yeet.rbxm
```

## Install locally (dev loop)

Yeet can't sync itself yet, so we push the built `.rbxm` into Studio's plugins folder by hand. From the repo root:

```
./scripts/sync-plugin.ps1
```

That runs `rojo build` and copies the result to `%LOCALAPPDATA%\Roblox\Plugins\Yeet.rbxm`. Reload in Studio via **Plugins → Manage Plugins → Reload** (or close and reopen the place).

## Layout

```
src/
  init.server.luau     -- root plugin script: toolbar + widget wiring
  ui/
    Widget.luau        -- DockWidget with Connect button + log panel
  net/
    Connection.luau    -- thin wrapper over the Studio-native WebSocket API
```

## WebSocket API caveat

[Connection.luau](src/net/Connection.luau) assumes an API shaped like `HttpService:WebSocketConnect(url)` returning an object with `MessageReceived`/`Closed` signals and `:Send`/`:Close` methods. The exact surface of Studio's native WebSocket support hasn't been fully nailed down in this project yet — **verify against current Roblox docs before running**. If the real API differs, adjust only `Connection.luau`; the rest of the plugin is isolated from the raw API.

## Wire format

JSON during Phase 0 (via `HttpService:JSONEncode`/`JSONDecode`). Phase 1 moves the main data path to MessagePack; the daemon side already has `rmp-serde` declared.
