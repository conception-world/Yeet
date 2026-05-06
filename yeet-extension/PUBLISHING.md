# Publishing to VS Code Marketplace — checklist

Before running `vsce publish`, walk through this list. Skipping any
step here either breaks the publish (`vsce publish` errors out) or
ships an extension with broken links visible in the Marketplace
listing.

## 1. Replace the placeholder publisher name

Open `package.json`. Today the `publisher` field reads:

```json
"publisher": "yeet-dev-TODO-replace-before-publish",
```

Replace with the **exact** publisher slug you registered at
https://marketplace.visualstudio.com/manage/publishers. If you
haven't registered yet, do that first — it's free and takes a
couple of minutes via Microsoft account.

```json
"publisher": "your-actual-publisher-slug",
```

If you `vsce publish` with the placeholder string, it fails with
"publisher not found" — no extension goes live, but the error
log clearly points at this field.

## 2. Update repository / bugs / homepage URLs

The `package.json` `repository`, `bugs`, and `homepage` fields all
point at `https://github.com/yeet-dev/yeet`. If your real public
repo lives elsewhere (different org, different name), update all
three. The Marketplace listing renders these as live links in the
sidebar; broken links erode user trust on day one.

```json
"repository": { "type": "git", "url": "https://github.com/<org>/<repo>.git" },
"bugs": { "url": "https://github.com/<org>/<repo>/issues" },
"homepage": "https://github.com/<org>/<repo>#readme",
```

The README also references `https://github.com/yeet-dev/yeet/releases`
in the "Daemon binary" section — `grep -rn "yeet-dev/yeet"` to find
every site and update consistently.

## 3. Verify `bin/win-x64/yeet-daemon.exe` is present and current

The bundled daemon is what makes a fresh-install experience zero-
setup on Windows. From the repo root:

```
cd yeet-daemon
cargo build --release
cp target/release/yeet-daemon.exe ../yeet-extension/bin/win-x64/
```

Without this binary, every Windows user who installs from the
Marketplace sees a "no daemon binary available" error on first
start. Confirm `bin/win-x64/yeet-daemon.exe` exists and that its
timestamp matches your last `cargo build --release`.

## 4. Code-signing (optional but strongly recommended)

The Windows binary is **not** code-signed. SmartScreen will warn
"Windows protected your PC" on first launch, scaring corporate
users into clicking "Don't run". Long-term fix: sign with an
Authenticode certificate (~$300/year via DigiCert / Sectigo).

Short-term workaround is documented in README.md (Requirements →
Windows: SmartScreen warning), but losing 10-30% of installs to
SmartScreen anxiety is a real cost on a polished public extension.

## 5. Smoke-publish via `--dry-run`

Run `vsce publish --dry-run` (or `vsce package` and inspect the
.vsix manually) before the real publish. The dry-run validates:

- publisher exists and matches the slug
- repository URLs are reachable
- the bundled .vsix size is reasonable
- no required field is missing

If the dry-run passes cleanly, `vsce publish` (without `--dry-run`)
ships the extension live.

## 6. Tag the release in git

```
git tag v0.3.0
git push origin v0.3.0
```

Helps users on the CHANGELOG follow exact commits, and lets the
GitHub Releases page hold the macOS / Linux pre-built binaries
for users who can't use the bundled win-x64 one.
