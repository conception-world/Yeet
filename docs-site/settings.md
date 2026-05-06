# Settings reference

Yeet has settings on both sides — the **VS Code extension** (in your
IDE settings JSON) and the **Studio plugin** (in the plugin's
Settings dock). Most users won't need to touch any of them; defaults
work for the common case.

## Extension settings

Edit via **File → Preferences → Settings**, search "yeet", or via
your `settings.json` directly.

### `yeet.daemonPath`

- **Type**: `string`
- **Default**: `""` (use bundled daemon)
- **Scope**: machine / user (workspace settings cannot override)

Override the path to a custom-built `yeet-daemon` binary. Leave empty
to use the bundled one (recommended).

```json
{
  "yeet.daemonPath": "/Users/me/Yeet/yeet-daemon/target/release/yeet-daemon"
}
```

::: warning Security
This setting is intentionally **machine-scoped**, not workspace-
scoped. Otherwise opening a hostile repo with a `.vscode/settings.json`
that pointed `yeet.daemonPath` at a malicious binary would silently
execute attacker code. With machine scope, the user explicitly opts
in once.
:::

### `yeet.debugEcho`

- **Type**: `boolean`
- **Default**: `false`

Pass `--debug-echo` to the daemon. Every hash and echo-detection
decision is logged at info level in the Output channel ("Yeet"
dropdown). Useful when diagnosing ping-pong loops between Studio and
disk.

Off by default because the log volume is heavy.

### `yeet.createTemplate`

- **Type**: `string[]`
- **Default**:

  ```json
  [
    "ServerScriptService",
    "ReplicatedStorage",
    "ReplicatedFirst",
    "ServerStorage",
    "StarterPlayer/StarterPlayerScripts",
    "StarterPlayer/StarterCharacterScripts"
  ]
  ```

Service scopes that **`Yeet: Create`** scaffolds into `src/`. Use
slashes to nest under a service (e.g. `StarterPlayer/StarterPlayerScripts`).
Empty array → no `src/` folders generated, just `default.project.json`.

## Plugin settings

Open the Yeet plugin dock in Studio → click **Settings** (top-right).
Settings persist across Studio restarts via `plugin:SetSetting`.

### Auto-connect on plugin load

- **Default**: off

When on, the plugin clicks **Connect** for you once it finishes
loading. Useful for users who always want sync on; removes one click
from the daily flow. Off by default so a fresh install never opens a
network connection without consent.

### Skip HTTP-enabled check

- **Default**: off

Skips the `HttpService.HttpEnabled` pre-flight check before
connecting. Power-users who use HttpService for other systems can
suppress the warning and the extra check. If HTTP is actually
disabled, the connection still fails — just later, with the daemon's
generic error instead of the specific "enable HTTP" message.

### Verbose logging

- **Default**: off

When on, the plugin's Activity log accepts trace-level breadcrumbs
(echo decisions, frame sizes, watcher debounce ticks). Off keeps the
log focused on user-meaningful events.

### Auto-confirm bulk sync

- **Default**: off

When on, the BulkSyncPreview dock auto-clicks **Apply Sync** on the
daemon's default resolutions instead of waiting for you. Intended
for headless / CI flows; **a typical user should leave this off so
they review every bulk apply.**

### Daemon URL override

- **Default**: `ws://127.0.0.1:34872`

Override for the daemon's WebSocket URL. Devs running a second
daemon on a custom port can point the plugin at it without
recompiling. Don't change this unless you know exactly why.

## Resetting

The plugin's Settings dock has a **Reset to defaults** button that
clears every persisted value back to the defaults above. Useful when
you've toggled too many things and want a clean slate.

For the extension, deleting the `yeet.*` keys from your `settings.json`
restores defaults — there's no separate reset.
