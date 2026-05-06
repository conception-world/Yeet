# Yeet — Security & Threat Model

## What Yeet protects against

Yeet is a developer tool that bridges Roblox Studio and your IDE
via a localhost WebSocket. The threats it actively mitigates are:

### 1. Browser-driven attacks (drive-by, DNS rebinding)

Any web page open in a browser can attempt to connect to local
ports. Without defenses, an attacker page hosted on
`https://evil.example.com` could open a WebSocket to
`ws://127.0.0.1:34872` and read every script Yeet syncs.

**Defense.** The daemon enforces an `Origin` header allowlist
during the WebSocket upgrade ([`yeet-daemon/src/main.rs:2129-2186`](yeet-daemon/src/main.rs#L2129-L2186)).
Browsers always attach an `Origin` header derived from the
current page URL; the daemon rejects any value matching
`http(s)://<remote-host>`. Loopback origins (`http://localhost`,
`http://127.0.0.1`, the IPv6 loopback) and the `null` origin used
by file:// pages and sandboxed iframes are accepted, since legit
clients (the Yeet plugin via Studio's HttpService, the Yeet
extension via Node `ws`, custom CLI tools) don't send a remote
Origin.

DNS rebinding is the most realistic browser-shaped attack: a
malicious page on `evil.com` resolves a subdomain to `127.0.0.1`
and uses the user's browser as a confused deputy. The Origin
allowlist still catches this — the page's Origin remains
`https://evil.com`, which is rejected even though the IP rebound
to loopback.

### 2. Resource exhaustion / DoS

Without caps, an attacker who can connect (browser via #1, or any
local process — see "What Yeet does NOT protect against" below)
could spam the daemon with massive frames or open many concurrent
sockets, exhausting memory and CPU.

**Defenses.**

- `WS_MAX_FRAME_BYTES = 16 MiB` ([`yeet-daemon/src/main.rs:50-65`](yeet-daemon/src/main.rs#L50-L65))
  caps a single WebSocket frame's size. Large enough for the
  legitimate 10 MiB content payload + JSON envelope, small
  enough that 4 attacker frames can't blow past 64 MiB total.
- `MAX_CONCURRENT_CONNECTIONS = 4` ([`yeet-daemon/src/main.rs:66-77`](yeet-daemon/src/main.rs#L66-L77))
  caps simultaneous WebSocket clients. Real usage tops at
  ~2 (one plugin + one extension); the cap leaves headroom for
  reconnect overlap without letting a flood saturate.
- `RATE_LIMIT_CAPACITY = 100 frames/sec` ([`yeet-daemon/src/main.rs:69-76`](yeet-daemon/src/main.rs#L69-L76))
  per-connection token bucket bounds the CPU cost of malicious
  clients spamming small frames.
- `WS_IDLE_TIMEOUT_SECS = 600` (10 min) ([`yeet-daemon/src/main.rs:77-87`](yeet-daemon/src/main.rs#L77-L87))
  drops connections that go silent — kills wedged sockets the
  kernel's TCP keepalive would otherwise hold for hours.

### 3. Path traversal / sandbox escape

A malicious or buggy client could send file paths like
`../../../etc/passwd` to overwrite arbitrary files outside the
project root.

**Defenses.**

- [`resolve_inside`](yeet-daemon/src/state.rs#L511) rejects `..`,
  absolute paths, NUL bytes, Windows UNC prefixes, and resolves
  symlinks before the write to detect symlink-escape.
- [`is_under_mapping`](yeet-daemon/src/state.rs#L422) further
  restricts writes to paths under a declared `$path` mapping in
  `default.project.json`. This blocks writes to project-root
  scaffolding files like `default.project.json` itself,
  `README.md`, `.yeet/auth-token` even if the path technically
  resolves inside the project.

These checks apply to every wire-driven write path:
`FileChanged`, `FileCreated`, `ConflictResolved`,
`ConflictResolvedManual`, `StudioSnapshotReport`.

### 4. Stale / forged auth tokens

When the daemon restarts, it generates a fresh auth token. A
plugin holding a stale token from the previous instance must
not be rejected silently — that would deadlock the user into
"Connect button does nothing" with no actionable error.

**Behaviour.** The auth gate is best-effort
([`yeet-daemon/src/main.rs:2293-2384`](yeet-daemon/src/main.rs#L2293-L2384)).
Mismatch on the supplied token logs a warning and re-issues
the current token via `AuthGranted` so the client can refresh
its stored value. No connection is dropped purely on auth.

This trades a small reduction in security ceremony for a major
UX win: there's no "stuck disconnected" state caused by a
forgotten token rotation. The actual security comes from the
Origin allowlist (#1) and process-level isolation (out of scope
— see below).

## What Yeet does NOT protect against

These threats are **out of scope** by design. Make sure your
setup matches the assumed environment.

### Other processes running as the same user on the same machine

Yeet listens on `127.0.0.1`. Anything else running as your user
account can connect to the same port: a curl script, a malicious
npm package, a browser extension running natively, another IDE
plugin, etc. The daemon's auth gate is best-effort (see #4
above) and accepts unauthenticated local clients with a warning.

If your threat model includes "another process on this same
machine, running as me, is malicious", you need OS-level
isolation that Yeet doesn't provide. Options:

- Run Yeet inside a dedicated VM or container.
- Run Yeet inside a sandboxing tool (firejail on Linux, etc.).
- Stop Yeet (`Yeet: Stop`) when you're not actively using it.

### Other users on the same machine

Multi-user shared workstations (lab machines, container
co-tenancy, jump hosts with multiple SSH sessions) are NOT a
supported environment. The pairing breadcrumb at
`<root>/.yeet/pairing` and the auth token at
`<root>/.yeet/auth-token` are only as private as the project
folder's filesystem permissions, which on most setups are
world-readable by default for files in `~`.

If you need to run Yeet on a shared machine, set tighter perms
on the project folder yourself (`chmod 700 <project_root>`) or
move the project to a private directory.

### Compromise of the developer's account

If an attacker has your user credentials, they can read your
files, start Yeet, and act as you. Yeet is a productivity tool,
not a defense against compromised credentials. Your wider
endpoint security (full-disk encryption, MFA on git/cloud,
locked screens, etc.) is what protects against that.

### Supply chain (the daemon itself, the extension itself)

If the published `.vsix` or pre-built daemon binary is itself
malicious — either through publisher account compromise or a
tampered GitHub Release artifact — Yeet has no defense against
its own code.

Mitigations from the user's side:

- Verify the publisher slug on the VS Code Marketplace listing
  matches the one we publish to (see this repo's README).
- For pre-built binaries: verify the SHA256 published on the
  GitHub Releases page matches what you downloaded.
- For source builds: check git tags are GPG-signed (when we
  start signing them — currently roadmap).

## Reporting a vulnerability

If you find a security bug, please **do not** open a public
GitHub issue. Email security details directly to the maintainers
listed in the project's GitHub profile, or use GitHub's private
security advisory feature (Security tab → "Report a vulnerability").
We aim to respond within 5 business days.

For non-security bugs (sync glitches, UI issues, crashes that
don't expose data), open a normal GitHub issue.
