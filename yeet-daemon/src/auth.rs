//! Auth-token persistence + pairing breadcrumb.
//!
//! The daemon generates a 256-bit `auth_token` at startup (in
//! `state::ProjectState::bootstrap`) and writes it to
//! `<project_root>/.yeet/auth-token` with permission `0o600` on Unix
//! and ACL-restricted-to-owner on Windows. Clients prove they have
//! local-FS read access by reading the file and echoing the token in
//! `Hello.auth_token`.
//!
//! Plugin clients running inside Roblox Studio can't read arbitrary
//! files, so they go through a pairing dance: the user runs `Yeet:
//! Pair Studio` in their IDE, which writes a `<project_root>/yeet-
//! pairing.txt` breadcrumb (timestamp inside, TTL 60s); on the next
//! `PairRequest` from the plugin, the daemon checks the breadcrumb
//! is fresh and replies with `AuthGranted` carrying the token. Plugin
//! stores the token in `plugin:SetSetting("yeet.authToken", ...)` so
//! subsequent reconnects skip the pairing.
//!
//! Threat model: this stops browsers (via Origin allowlist in main)
//! and naïve local processes that can't read the project root or
//! create the breadcrumb. It does NOT stop a malicious local process
//! with the same user privileges as the daemon — that process can
//! read the token file directly and bypass everything. We accept
//! this; "user-level malware on the same box" is out of scope. The
//! defense-in-depth value here is that an attacker has to actively
//! find and read a per-project file rather than just open a socket.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// `<root>/.yeet/auth-token` — daemon-generated token persisted at
/// startup, read by extensions.
pub const AUTH_TOKEN_FILENAME: &str = "auth-token";
/// `<root>/.yeet/pairing` — breadcrumb the extension creates and
/// keeps fresh while the daemon runs (auto-pair flow). File
/// contents: a single line with unix-time-seconds. Daemon trusts
/// this clock; clock skew between the extension and daemon is
/// minimal (same machine, same OS clock).
///
/// Lives inside `.yeet/` (alongside `auth-token` and the persistent
/// base tree) so it's hidden from the user's file tree and reliably
/// gitignored — `Yeet: Create` writes a `.gitignore` that excludes
/// `.yeet/`, so the breadcrumb never gets accidentally committed.
/// Earlier versions kept it at `<root>/yeet-pairing.txt` which led
/// to it appearing in `git status` and churning CI on every refresh.
pub const PAIRING_BREADCRUMB_FILENAME: &str = "pairing";
/// How long a `yeet-pairing.txt` stays valid for. 60s gives the user
/// time to alt-tab to Studio and click "Try pairing" without being
/// so wide it's a meaningful attack window. After this, the daemon
/// rejects `PairRequest` and the user has to re-run the IDE command.
pub const PAIRING_BREADCRUMB_TTL_SECS: u64 = 60;

/// Persists `token` to `<root>/.yeet/auth-token`. Creates the
/// `.yeet/` directory if missing. On Unix sets perm `0o600`
/// (owner-read-write only); on Windows the file inherits the
/// `.yeet/` directory's ACL — for a project under `%USERPROFILE%`
/// that's already user-only by default.
pub fn write_auth_token_file(root: &Path, token: &str) -> Result<()> {
    let dir = root.join(".yeet");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(AUTH_TOKEN_FILENAME);
    std::fs::write(&path, token)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(&path, perm)
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

/// Resolves the breadcrumb's full on-disk path. Centralised so the
/// breadcrumb's home (`<root>/.yeet/pairing`) is the same in every
/// caller — earlier versions had the path inlined in three places
/// and a refactor missed one, leading to "wrote here, looked there"
/// pair failures.
fn breadcrumb_path(root: &Path) -> std::path::PathBuf {
    root.join(".yeet").join(PAIRING_BREADCRUMB_FILENAME)
}

/// Returns true iff `<root>/.yeet/pairing` exists AND its
/// timestamp is within `PAIRING_BREADCRUMB_TTL_SECS`. Anything older
/// is treated as stale (the file may be left over from a prior
/// session). Returns false for any error reading or parsing the
/// file — failing closed is the right policy for an auth gate.
pub fn pairing_breadcrumb_valid(root: &Path) -> bool {
    let path = breadcrumb_path(root);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let line = body.trim();
    let written_at: u64 = match line.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return false,
    };
    // `written_at` could be in the future (clock skew, malicious
    // file, etc.) — saturating sub keeps the comparison safe and
    // future timestamps are effectively age = 0 (still inside TTL).
    let age = now.saturating_sub(written_at);
    age <= PAIRING_BREADCRUMB_TTL_SECS
}

/// Best-effort delete of the pairing breadcrumb. Called after a
/// successful `PairRequest` so the same breadcrumb can't be reused
/// to pair a second client. Errors are logged at the call site;
/// failure here just means the next PairRequest within the TTL
/// would also succeed (no security impact, only mild UX confusion).
pub fn delete_pairing_breadcrumb(root: &Path) -> std::io::Result<()> {
    let path = breadcrumb_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // Already gone — fine.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Constant-time string comparison. `==` on `String` short-circuits
/// at the first mismatched byte, leaking timing about how many
/// leading bytes matched. For a 64-byte hex token this is mostly
/// theoretical — but this is the auth gate, so we use the constant-
/// time form anyway. Pure-Rust, no extra crate.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn constant_time_eq_handles_equal_and_diff() {
        assert!(constant_time_eq("hello", "hello"));
        assert!(!constant_time_eq("hello", "world"));
        assert!(!constant_time_eq("hello", "hell")); // length differs
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn pairing_breadcrumb_valid_recognizes_fresh_and_stale() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();

        // No file → invalid.
        assert!(!pairing_breadcrumb_valid(root));

        // Fresh file → valid. Mirrors what the extension does:
        // creates `<root>/.yeet/` then writes the breadcrumb inside.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::create_dir_all(root.join(".yeet"))?;
        let path = breadcrumb_path(root);
        std::fs::write(&path, now.to_string())?;
        assert!(pairing_breadcrumb_valid(root));

        // Stale file (TTL + 1 ago) → invalid.
        let old = now.saturating_sub(PAIRING_BREADCRUMB_TTL_SECS + 1);
        std::fs::write(&path, old.to_string())?;
        assert!(!pairing_breadcrumb_valid(root));

        // Garbage contents → invalid.
        std::fs::write(&path, "not a number")?;
        assert!(!pairing_breadcrumb_valid(root));

        // Future-timestamp file → still valid (clock skew tolerance).
        let future = now + 10;
        std::fs::write(&path, future.to_string())?;
        assert!(pairing_breadcrumb_valid(root));
        Ok(())
    }

    #[test]
    fn delete_breadcrumb_is_idempotent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        // Delete-when-missing is fine.
        delete_pairing_breadcrumb(root)?;
        // Create then delete.
        std::fs::create_dir_all(root.join(".yeet"))?;
        std::fs::write(breadcrumb_path(root), "0")?;
        delete_pairing_breadcrumb(root)?;
        assert!(!breadcrumb_path(root).exists());
        Ok(())
    }

    #[test]
    fn write_auth_token_file_creates_dir_and_writes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        write_auth_token_file(root, "abcdef0123456789")?;
        let read = std::fs::read_to_string(root.join(".yeet").join(AUTH_TOKEN_FILENAME))?;
        assert_eq!(read, "abcdef0123456789");
        // Sanity: rewriting works (idempotent).
        write_auth_token_file(root, "newtoken")?;
        assert_eq!(
            std::fs::read_to_string(root.join(".yeet").join(AUTH_TOKEN_FILENAME))?,
            "newtoken"
        );
        let _ = Duration::from_millis(0); // touch unused-import lint
        Ok(())
    }
}
