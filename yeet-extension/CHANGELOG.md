# Changelog

All notable changes to the Yeet VS Code extension are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] — Initial public release

### Added
- Bidirectional sync between Roblox Studio scripts and on-disk Luau
  files via a local Rust daemon listening on `127.0.0.1:34872`.
- `Yeet: Start` / `Yeet: Stop` commands to manage the daemon
  process.
- `Yeet: Create` scaffolds a new Rojo-compatible project (`default.project.json`,
  `src/` mounts for ServerScriptService / ReplicatedStorage / etc.,
  `.gitignore`).
- `Yeet: Sync From Studio` / `Yeet: Sync From IDE` for one-shot
  bulk migrations.
- Auto-pair flow: extension keeps `<root>/.yeet/pairing` fresh while
  the daemon runs so the Studio plugin connects on first click with
  zero manual pairing.
- Folder-open confirmation modal: every wire-driven `vscode.openFolder`
  request requires explicit user approval (defends against malicious
  daemons / forged URI handlers).
- `yeet.daemonPath` is `scope: "machine"` — workspace settings can't
  override it (defends against repos with hostile `.vscode/settings.json`).
- WebSocket Origin allowlist on the daemon: only loopback / native
  clients accepted; browsers / DNS-rebinding rejected at the upgrade.
- Concurrent connection cap (4) on the daemon to bound peak attacker
  memory under DoS attempts.

### Security
- `yeet.daemonPath` validated as executable (`fs.accessSync(X_OK)`)
  before spawn — surfaces clear errors instead of cryptic
  `EACCES`/`ENOEXEC`.
- Auth token (256-bit, `<root>/.yeet/auth-token`, perm `0600` on
  Unix) optional but validated when present.
- Daemon enforces frame-size cap (16 MiB) to bound buffering.

### Fixes
- Origin check no longer rejects loopback HTTP origins (Studio's
  WebStreamClient sends one).
- Plugin auth handlers correctly reference `self.plugin` instead of
  the bare `plugin` global (the global is only valid in
  `init.server.luau`).
- Heartbeat first ping waits one full interval after Hello so the
  daemon's auth dance isn't interrupted by an early ping frame.
- `pcall` guards on every Roact event handler so a callback throw
  doesn't silently kill the Studio event-handling thread.
- `Settings.save` now returns a structured `SaveResult` and the
  Studio Settings dock surfaces a red error banner if any field
  fails to persist — the previous green "✓ Settings saved" banner
  could appear even when `plugin:SetSetting` silently dropped the
  write, leaving toggles flipped back to their defaults.
- `Widget.new` accepts persisted settings via the constructor and
  applies them via `task.defer` after `Roact.mount` — closes the
  startup race where consumers (auto-connect, Activity logger) could
  read settings before the Widget's internal fields existed.
- Re-probe of `127.0.0.1:34872` immediately before `spawn(daemon)`
  closes the ~50–200 ms race window where an antivirus / second VS
  Code window / leftover daemon could bind the port between the
  initial probe and our bind. Modal surfaces the cause in either
  case (re-probe hit, or daemon exited within 2 s).
- Daemon-spawn early-exit detection: stderr is buffered (last 20
  lines) and surfaced in a focused modal when the daemon exits
  within 2 s of spawn. Most common cause is "Address already in
  use", which the user couldn't see without manually opening Output.
- Reconnect budget is more patient: 30 attempts with 60 s ceiling
  (~30 min) instead of the previous 10 × 30 s (~5 min) before the
  give-up notification, so transient WiFi / VPN / Studio-reload
  flaps no longer false-trigger the fatal modal.
- Bundled-daemon version mismatch detection: extension parses the
  daemon's startup version banner and warns (non-modal) if it
  differs from `EXPECTED_DAEMON_VERSION`. Catches the common dev
  case of `yeet.daemonPath` pointing at a stale custom build.
- `openProject` deduplicates concurrent prompts via a module-level
  latch — a buggy daemon (or malicious URI) emitting back-to-back
  open requests can no longer pile up modals that block the IDE.

### Known limitations (deferred to v0.3.1)
- macOS / Linux daemon binaries are not bundled (Windows-only this
  release; macOS/Linux users build from source or download from the
  GitHub Releases page).
- Windows daemon is not code-signed (SmartScreen warning on first
  run; click "More info → Run anyway" to proceed).
- No automated test suite for the extension itself (the Rust daemon
  ships with 131 passing tests).
- Multi-place / multi-window Studio connections share the auth
  token via `plugin:SetSetting` (race possible but rare).
- `daemonUrl` setting accepts any string (no `ws://` validation).
- Legacy `yeet-pairing.txt` from earlier dev builds is not cleaned
  on upgrade (harmless; lives in `.yeet/`).
