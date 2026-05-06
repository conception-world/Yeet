# Yeet — Release Guide

Step-by-step guide for publishing the **plugin** to Roblox Creator Hub
and the **extension** to the VS Code Marketplace. Read top-to-bottom
the first time; the checklist at the end is your reusable cheat sheet.

---

## Pre-flight

Before doing anything else, complete these one-time setup tasks. They
gate the rest of the guide.

### 0. Decide what you're publishing as

Pick a **publisher slug** for the VS Code Marketplace and a **creator
account** for Roblox. They don't have to match — pick what makes
sense for the brand you're going to commit to. Examples:

- Marketplace: `your-name`, `studio-name`, `org-name`
- Roblox: your developer account or a group you own

Once you publish under a slug, you're stuck with it (renaming
requires a new listing). Pick something you're OK with for years.

### 1. Create the GitHub repo

Repo location is referenced in `package.json` (`repository`, `bugs`,
`homepage`), `SECURITY.md`, `README.md`, and the daemon's error
messages. **All these references currently point at
`https://github.com/yeet-dev/yeet` which doesn't exist.**

Steps:

1. Create a public GitHub repo. Name it `yeet` (or your preferred
   short slug).
2. Push the project: `git init && git remote add origin
   https://github.com/<your-org>/<repo>.git && git add . && git
   commit -m "initial public release" && git push -u origin main`.
3. **Replace every `yeet-dev/yeet` reference** in:
   - `yeet-extension/package.json` (`repository.url`, `bugs.url`,
     `homepage`)
   - `yeet-extension/README.md` (Daemon binary section,
     "Issues / contributions" section)
   - `yeet-extension/CHANGELOG.md` (release URL footnotes if any)
   - `yeet-extension/PUBLISHING.md`
   - `yeet-extension/src/extension.ts:328` (`releasesUrl` constant
     in the bundled-missing error message)
   - `SECURITY.md` (the repo-link references at the top)

   `grep -rln "yeet-dev/yeet" .` finds every site.

### 2. Create the Marketplace publisher account

1. Go to <https://marketplace.visualstudio.com/manage/publishers>.
2. Sign in with a Microsoft account.
3. Click **New publisher**. Pick the slug you decided on in step 0.
4. Fill the display name, description, etc. — these appear on every
   extension you publish.
5. Generate a **Personal Access Token (PAT)** at
   <https://dev.azure.com/<your-publisher>/_usersSettings/tokens>:
   - Organization: All accessible organizations
   - Scopes: **Marketplace → Manage**
   - Save it somewhere private; you'll use it with `vsce login`.

### 3. Replace the placeholder publisher in `package.json`

Open `yeet-extension/package.json`. The current value is the
intentionally-broken sentinel:

```json
"publisher": "yeet-dev-TODO-replace-before-publish",
```

Replace `yeet-dev-TODO-replace-before-publish` with your actual
publisher slug from step 2. **The placeholder is designed to fail
`vsce publish`** — if you forget this step, the publish errors out
with "publisher not found" instead of silently shipping a broken
listing.

### 4. (Optional but strongly recommended) Code-sign the Windows daemon

Unsigned `.exe`s trigger Windows SmartScreen warnings on first
launch. Roughly 10-30% of corporate / cautious users click "Don't
run" rather than "More info → Run anyway", losing them as users.

If you can budget it:

1. Buy an **Authenticode certificate** from DigiCert, Sectigo, or
   Certera (~$300/year for a standard cert; EV is more for instant
   trust).
2. Sign `bin/win-x64/yeet-daemon.exe` with `signtool sign /tr
   http://timestamp.digicert.com /td sha256 /fd sha256 /a
   yeet-daemon.exe`.
3. Verify with `signtool verify /pa /v yeet-daemon.exe`.
4. Re-bundle into the .vsix.

If you can't budget it now: leave as-is, but make sure the README's
"Windows: SmartScreen warning" section is prominent. A v1.1
post-launch task can revisit signing.

### 5. (Optional) Cross-compile for macOS / Linux

Currently only `bin/win-x64/yeet-daemon.exe` ships. macOS / Linux
users either build from source or download from GitHub Releases.

If you want to ship them:

- Need access to a macOS machine (M-series chip for arm64) or a
  cross-compile toolchain (`cargo zigbuild`, GitHub Actions runners,
  etc.). Linux x64 is easy to cross-compile from Windows.
- Build once per target and drop the binary into:
  - `yeet-extension/bin/macos-arm64/yeet-daemon`
  - `yeet-extension/bin/macos-x64/yeet-daemon`
  - `yeet-extension/bin/linux-x64/yeet-daemon`
- macOS binaries should be signed + notarized; otherwise Gatekeeper
  blocks first-launch (worse UX than Windows SmartScreen because the
  user must explicitly `xattr -d com.apple.quarantine` to bypass).
- Update README to remove the "Windows-only bundled" caveat.

If you can't ship cross-platform binaries this release: GitHub
Releases is the fallback. Build the daemon for each platform
manually, attach to a Release tagged `v0.3.0`, and let macOS / Linux
users download + set `yeet.daemonPath` themselves. The README and
the bundled-missing error message both point them there.

---

## Publishing the **VS Code Extension**

### 1. Final pre-publish checks

```bash
cd yeet-extension

# Verify TypeScript compiles cleanly.
npm run typecheck

# Verify the bundled daemon binary is fresh.
ls -la bin/win-x64/yeet-daemon.exe
# If older than your last `cargo build --release`, rebuild:
#   cd ../yeet-daemon && cargo build --release
#   cp target/release/yeet-daemon.exe ../yeet-extension/bin/win-x64/

# Verify package.json has the real publisher slug.
grep '"publisher"' package.json
# Should NOT contain "TODO". If it does, go back to Pre-flight step 3.

# Verify repo URLs are real.
grep -E 'github.com' package.json README.md
# Should point to your real repo, not yeet-dev/yeet.
```

### 2. Build and inspect the .vsix

```bash
# Install vsce if you haven't.
npm install -g @vscode/vsce

# Login with the PAT from Pre-flight step 2.
vsce login <your-publisher-slug>
# Paste the PAT when prompted.

# Build the .vsix WITHOUT publishing yet.
vsce package
# Output: yeet-0.3.0.vsix (or similar)

# Inspect what's inside.
unzip -l yeet-0.3.0.vsix | head -40
# CONFIRM you see:
#   bin/win-x64/yeet-daemon.exe (~3.9 MB)
#   out/extension.js (compiled)
#   package.json
#   LICENSE, README.md, CHANGELOG.md
# CONFIRM you DON'T see:
#   src/*.ts (excluded by .vscodeignore)
#   node_modules/* (excluded)
```

### 3. Test-install locally

Test the .vsix in a clean VS Code instance before publishing:

```bash
# Uninstall any dev version first.
code --uninstall-extension <publisher>.yeet

# Install the freshly-built .vsix.
code --install-extension yeet-0.3.0.vsix
```

In a fresh VS Code window:

1. Open a folder with `default.project.json` (the test project).
2. Run **Yeet: Start** — daemon should spawn from the bundled binary.
   Status bar shows `Yeet: running`.
3. Open Studio with the matching plugin `.rbxm` installed → click
   Connect — should connect on first try.
4. Run **Yeet: Stop** → daemon shuts down cleanly.
5. Run `Yeet: Start` twice in quick succession → second invocation
   shows "Yeet daemon is starting" instead of double-spawning.
6. Test on a workspace without `default.project.json` and run
   **Yeet: Create** — extension should activate via `onCommand:`
   and scaffold the project.

If anything fails, fix and re-run `vsce package`.

### 4. Dry-run publish

```bash
vsce publish --pre-release  # if you want to mark it as pre-release
# OR
vsce publish               # for a stable release
# Add --no-dependencies if you have any private deps you don't want to verify.
```

`vsce publish` validates publisher + repo URLs + package contents
and uploads. The Marketplace page goes live within 5-10 minutes.

### 5. Verify the listing

After ~10 minutes, visit your extension's Marketplace page:
`https://marketplace.visualstudio.com/items?itemName=<publisher>.yeet`

Click through:
- Install button works
- README renders correctly
- Repo / bugs / homepage links navigate correctly
- Version number matches what you published

If something looks wrong, bump the version in `package.json` (e.g.,
`0.3.1`), fix, and `vsce publish` again. You CANNOT delete a
published version, only deprecate it.

### 6. Tag the release in git

```bash
git tag v0.3.0
git push origin v0.3.0
```

Optionally: create a GitHub Release at
`https://github.com/<your-org>/<repo>/releases/new` with the
v0.3.0 tag. Attach pre-built daemon binaries for macOS/Linux if
you have them — that's where the "no daemon binary available"
error message points users.

---

## Publishing the **Roblox Studio Plugin**

The plugin can be distributed two ways. Pick one (or both).

### Option A: Roblox Creator Marketplace (recommended for reach)

1. Open Roblox Studio.
2. In the toolbar, **Plugins → Plugins Folder** → drop
   `yeet-plugin/build/yeet-plugin.rbxm` into the open folder if it
   isn't there already (or wherever you've been building).
3. Right-click the plugin in the Plugins panel → **Save as Local
   Plugin** if needed.
4. Go to <https://create.roblox.com/store> and sign in.
5. Click **Create** → **Plugin**.
6. Upload the `.rbxm`. Fill out:
   - **Name**: Yeet — Roblox Studio ↔ IDE sync
   - **Summary** (short): "Bidirectional sync between Roblox Studio
     and your IDE (VS Code, Cursor, Antigravity)."
   - **Description** (long): paste from `yeet-extension/README.md`,
     adjust to plugin perspective.
   - **Icon** (512×512 PNG): you don't have one yet — design one
     before listing, or use a temporary text-based icon.
   - **Tags**: `sync`, `rojo`, `ide`, `code`, `developer-tools`
   - **Privacy**: Public
   - **Distribution**: Free (or paid; see Roblox's monetization docs)
7. Submit for review. Roblox's plugin moderation typically takes
   1-3 business days.

### Option B: GitHub Releases (always available)

Even if you publish to the Marketplace, also drop the `.rbxm` into
your GitHub Release for the v0.3.0 tag. Some users prefer manually
installing plugins they trust, and the README for the extension
already points users at the Releases page.

```bash
# After you've done git tag v0.3.0 + git push:
# Visit https://github.com/<your-org>/<repo>/releases/new
# Tag: v0.3.0
# Title: Yeet v0.3.0 — Initial Public Release
# Description: paste CHANGELOG.md content for v0.3.0
# Attachments:
#   - yeet-plugin.rbxm  (drag from yeet-plugin/build/)
#   - yeet-daemon-windows-x64.exe  (rename from target/release/yeet-daemon.exe)
#   - yeet-daemon-macos-arm64       (if you have it)
#   - yeet-daemon-macos-x64          (if you have it)
#   - yeet-daemon-linux-x64          (if you have it)
```

### Option C: Self-hosted

Skip the Marketplace entirely; users download the .rbxm from your
GitHub Release and drop it into their Roblox Studio Plugins folder
manually. Simplest for you, friction for users. Acceptable if your
audience is technical (devs who already know the workflow).

---

## Post-launch monitoring

In the first 48 hours after publish:

1. **Watch GitHub Issues** for crash reports, especially:
   - "no daemon binary available" (macOS/Linux user) — point them
     at the Releases page.
   - "Connect button does nothing" — likely a bug we missed; ask
     for the Output channel content.
   - "Settings not saving" — known semi-broken; if reports are high,
     prioritize the daemon-mediated persistence (Step 4 of the
     Settings investigation plan in this repo's plan file).
2. **Watch Marketplace reviews** on the extension page. The first
   handful of reviews shape your average rating; respond to negative
   ones promptly.
3. **Watch Roblox Plugin reviews/comments** on the Marketplace
   listing. Roblox users tend to comment in-listing rather than
   open GitHub issues.
4. **Pin a "Known Issues" issue in GitHub** linking the v1.1 backlog
   so users know the cosmetic / edge-case problems are tracked.

---

## v1.0 → v1.1 backlog (post-launch fixes)

Things to address in the first patch release based on the audit.
The 3 CRITICAL and 3 HIGH items have been closed in the v0.3.0
release prep cycle; remaining MEDIUM items move to v0.3.1.

| Severity | Issue | Where | Status |
|---|---|---|---|
| ~~CRITICAL~~ | ~~Settings JSON-encode failure → partial save without warning~~ | `yeet-plugin/src/ui/Settings.luau` | DONE — `SaveResult` returns per-field errors; SettingsDock renders red banner |
| ~~CRITICAL~~ | ~~`Settings.load` race with autoConnect → potential nil deref~~ | `yeet-plugin/src/init.server.luau` | DONE — `Widget.new` accepts `initialSettings`; apply runs under `task.defer` |
| ~~CRITICAL~~ | ~~`probeDaemonAlive` race → spawn after probe but before bind~~ | `yeet-extension/src/extension.ts` | DONE — re-probe before spawn + early-exit modal with stderr context |
| ~~HIGH~~ | ~~`MAX_RECONNECT_ATTEMPTS=10` too aggressive for flaky networks~~ | `yeet-extension/src/websocket.ts` | DONE — bumped to 30 attempts × 60s ceiling (~30 min budget) |
| ~~HIGH~~ | ~~Bundled daemon protocol mismatch not detected~~ | `yeet-extension/src/extension.ts` | DONE — stderr scraper compares daemon version against `EXPECTED_DAEMON_VERSION` and warns on mismatch |
| ~~HIGH~~ | ~~`openProject` modal blocks IDE if daemon spams requests~~ | `yeet-extension/src/openProject.ts` | DONE — `promptInFlight` latch dedups concurrent prompts |
| MEDIUM | `daemonUrl` accepts any string (no `ws://` validation) | `yeet-plugin/src/ui/SettingsDock.luau` | open |
| MEDIUM | Legacy `yeet-pairing.txt` not cleaned on upgrade | `yeet-extension/src/extension.ts` | open |
| MEDIUM | `fs.accessSync(X_OK)` weak on Windows NTFS | `yeet-extension/src/extension.ts` | open |
| MEDIUM | Settings persistence — needs daemon-mediated fallback if `plugin:SetSetting` proves unreliable across Studio versions | new daemon protocol round-trip | open — promote if user reports stack up |

The Settings persistence path is still the one to watch most carefully
post-launch — `plugin:SetSetting` reliability varies across Studio
builds. If reports come in, the daemon-mediated fallback is v0.3.1
priority.

---

## Reusable checklist

```
PRE-FLIGHT (one time)
[ ] Pick publisher slug, create GitHub repo, replace yeet-dev/yeet refs
[ ] Create VS Code Marketplace publisher + PAT
[ ] Replace package.json publisher field
[ ] (Optional) Code-sign Windows daemon
[ ] (Optional) Cross-compile macOS/Linux daemon binaries

EVERY RELEASE
[ ] cargo test --release        (yeet-daemon)        → 131 passing
[ ] cargo build --release       (yeet-daemon)
[ ] cp target/release/yeet-daemon.exe yeet-extension/bin/win-x64/
[ ] rojo build --output yeet-plugin/build/yeet-plugin.rbxm
[ ] cd yeet-extension && npm run typecheck
[ ] cd yeet-extension && vsce package
[ ] Manually install + smoke-test the .vsix in clean VS Code
[ ] vsce publish
[ ] Upload .rbxm to Roblox Marketplace (or GitHub Release)
[ ] git tag vX.Y.Z && git push origin vX.Y.Z
[ ] Create GitHub Release with .rbxm + daemon binaries attached
[ ] Update CHANGELOG.md for next version
```
