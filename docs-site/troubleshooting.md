# Troubleshooting

Common issues and how to fix them. If your problem isn't here, check
the
[GitHub Issues](https://github.com/conception-world/Yeet/issues).

## Plugin shows "Disconnected" and Connect does nothing

1. **Is the daemon running?** Look at VS Code's status bar — it
   should say `Yeet: running`. If not, run **`Yeet: Start`**.
2. **Is HTTP enabled in Studio?** File → Game Settings → Security →
   Allow HTTP Requests = on. This is per-place, not per-Studio.
3. **Open the plugin's Activity log** (button in the dock). It logs
   every connection attempt with the actual error from the
   WebSocket layer.

## Daemon says "Address already in use"

Port `34872` is held by another process. Most common causes:

- A second VS Code window also running Yeet
- A leftover `yeet-daemon.exe` from a crashed session
- An antivirus / network-monitoring tool that briefly squats on
  loopback ports

The extension's early-exit modal (added in v0.3.0) already names
this case. To fix:

```bash
# Windows
netstat -ano | findstr :34872
taskkill /PID <pid> /F

# macOS / Linux
lsof -i :34872
kill <pid>
```

Or close all VS Code windows, then start fresh.

## Windows SmartScreen blocks first launch

The bundled daemon `.exe` is not yet code-signed. On the first run,
Windows shows **"Windows protected your PC"**. Click:

1. **More info**
2. **Run anyway**

Windows remembers the decision per-binary; subsequent runs don't
prompt. Code signing is on the v0.3.1 roadmap.

## Settings dock toggle flips back to off

In v0.2.x and earlier, this happened because `plugin:SetSetting`
sometimes doesn't persist booleans across dock-reopen on certain
Studio builds. v0.3.0 wraps every value in
`HttpService:JSONEncode({v: ...})` (which Studio persists reliably)
and surfaces a **red error banner** if any field genuinely fails
to persist.

If you still see toggles flipping back on v0.3.0+:

1. Open Studio's Output channel (View → Output)
2. Toggle a setting → click **Save**
3. Look for `[Yeet:settings] wrote ... readback ok=...` lines
4. If readback shows `value=nil`, your Studio build's
   `plugin:SetSetting` is genuinely broken — open an issue with the
   Studio version. We have a daemon-mediated fallback queued for
   v0.3.1.

## "Yeet daemon version mismatch" warning

You set `yeet.daemonPath` to a custom-built binary that's at a
different version than the extension expects. Two fixes:

- **Rebuild the daemon**: `cargo build --release` in `yeet-daemon/`,
  then refresh the bundle: `cp target/release/yeet-daemon.exe ../yeet-extension/bin/win-x64/`
- **Clear the override**: delete `yeet.daemonPath` from your
  settings to fall back to the bundled daemon

Sync will usually still work (semver minor compat), but
protocol-level changes can cause subtle misbehavior.

## Plugin connects but bulk-sync interface doesn't appear

Sometimes Studio caches an old plugin layout and the BulkSync dock
ID isn't recognized. Two recovery paths:

- Click the **Show Sync** button in the toolbar (manual escape
  hatch added in v0.3.0)
- Reinstall the plugin: right-click the plugin file in Studio's
  Plugins folder → Reinstall

## Sync ping-pongs (edits keep echoing back and forth)

This is the echo-detection cache failing. It should self-correct in
&lt;1 s. If it persists:

1. Set `yeet.debugEcho` = true
2. Reload the daemon (`Yeet: Stop` → `Yeet: Start`)
3. Trigger a small edit and watch the Output channel for `isEcho`
   decisions
4. Open an issue with the log

## Reconnect "gave up after 30 attempts" notification

The control channel retries with exponential backoff for ~30 minutes
before showing the fatal modal. If you're seeing this, the daemon
has been unreachable for a long time. Check:

- Is the daemon process still alive? (`Yeet: Start` if not)
- Has the port changed? (`yeet.daemonPath` pointing at a daemon
  bound to a different port)
- Is a firewall blocking loopback? (rare but happens with corporate
  AVs)

Click **Restart Yeet** in the modal to reset the budget and try
again, or **Open Settings** to fix the path.

## Studio is slow / freezes during a 1000+ script bootstrap

The plugin renders bulk-sync previews progressively (100 rows per
frame), but the underlying snapshot can still take ~5–10 s on very
large projects. Wait for the dock to populate; if it doesn't after
30 s, file an issue.

## I want to wipe state and start fresh

Yeet's per-project state lives in `<project>/.yeet/`:

- `auth-token` — daemon's auth token (regenerated on next start)
- `pairing` — auto-pair breadcrumb (refreshed continuously while
  daemon runs)

Delete the `.yeet/` directory and restart `Yeet: Start`. The plugin
will re-pair on next connect.

The plugin's settings live in your Studio user profile (not the
project), so they survive a `.yeet/` wipe. Reset them via the
plugin's **Reset to defaults** button.

## Still stuck?

Open an issue with:

1. Your OS + Studio version
2. The extension version
3. The relevant log lines from VS Code's "Yeet" Output channel
4. The plugin's Activity log (rightmost button in the dock — copies
   to clipboard)

[New issue →](https://github.com/conception-world/Yeet/issues/new)
