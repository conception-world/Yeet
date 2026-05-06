//! Append-only JSONL audit log for production diagnosis.
//!
//! Tracing logs (stderr / OS journal) are sufficient for development but
//! disappear when the user closes the terminal, when the OS log rotates, or
//! when the daemon process dies. For production sync of published games we
//! need a persistent record that ties each disk/Studio mutation to the
//! session that caused it — so "why did my script disappear at 3am" has an
//! answer beyond "we don't know".
//!
//! One line per mutation, in `<project_root>/.yeet/audit.log`. The ring is
//! rotated to `audit.log.1` when the file crosses `MAX_BYTES`. Best-effort:
//! any I/O error is logged via `tracing::warn!` and swallowed — failing a
//! sync because the audit log couldn't be appended to would be worse than
//! losing one log line.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// File rotates when its size crosses this. One previous generation is kept
/// (`audit.log.1`) — older history rolls off. 50 MiB at ~250 bytes/line ≈
/// 200K events, ~1 week of typical interactive use.
const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// What changed. The variant names match `ServerMsg` / `ClientMsg` discriminator
/// strings where they overlap so audit and protocol logs join cleanly.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `tree_fs` updated from a watcher event (IDE → daemon side change).
    FsChanged,
    /// `tree_studio` updated from a plugin frame (Studio → daemon side change).
    StudioChanged,
    /// Daemon wrote to disk (Studio → IDE direction completing).
    FsWrite,
    /// Daemon removed from disk.
    FsDelete,
    /// Daemon told the plugin to write into Studio (IDE → Studio direction).
    StudioPush,
    /// Daemon told the plugin to delete a Studio instance.
    StudioDelete,
    /// User-resolved conflict committed.
    ConflictResolved,
    /// User abandoned conflicts (closed dock without resolving).
    ConflictAbandoned,
    /// Bulk sync apply, per resolution.
    BulkApply,
    /// A bulk sync apply failed mid-batch (per-entry).
    BulkFailure,
    /// Daemon rotated `session_id` (buffer overflow, plugin SessionEnd, or
    /// explicit fresh handshake). `path` carries the rotation reason; the
    /// `sha_before`/`sha_after` fields carry the old and new IDs so the
    /// post-mortem can correlate a "lost session" symptom to the cause.
    SessionRotated,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry<'a> {
    /// RFC3339 UTC. Computed at record() time so caller doesn't have to.
    pub ts: String,
    pub kind: Kind,
    pub path: &'a str,
    /// SHA-256 hex of the content prior to this event, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha_before: Option<&'a str>,
    /// SHA-256 hex of the content after this event, if known. Absent for
    /// deletions or aborted operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha_after: Option<&'a str>,
    /// Daemon `session_id` at time of event. Lets us correlate a sequence
    /// of changes to the plugin/extension session that caused them, even
    /// if multiple connections were churning through the daemon.
    pub session_id: &'a str,
    /// Optional free-form note (error message, resolution choice, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'a str>,
}

/// Returns the path to the audit log inside the project's `.yeet/` directory.
pub fn log_path(project_root: &Path) -> PathBuf {
    project_root.join(".yeet").join("audit.log")
}

/// Appends `entry` as one JSON line to the audit log. Best-effort: any I/O
/// failure is logged via `tracing::warn!` and otherwise ignored. Rotates
/// the log when it crosses `MAX_BYTES`.
pub fn record(project_root: &Path, entry: &Entry<'_>) {
    if let Err(e) = record_inner(project_root, entry) {
        tracing::warn!(error = ?e, "audit log write failed");
    }
}

fn record_inner(project_root: &Path, entry: &Entry<'_>) -> std::io::Result<()> {
    let path = log_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            // Rotate. A failure here is non-fatal — better to keep
            // appending to the oversized file than to lose an event.
            let rotated = path.with_extension("log.1");
            let _ = fs::remove_file(&rotated);
            let _ = fs::rename(&path, &rotated);
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    // serde_json::to_writer + manual newline avoids allocating an
    // intermediate String for what's typically a sub-1KB record.
    serde_json::to_writer(&mut file, entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Convenience for the common case: pulls `ts` from the system clock so
/// callers don't have to remember the format.
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Hand-rolled to avoid pulling chrono just for this. RFC3339 with
    // millisecond precision: 2026-04-27T15:32:14.123Z.
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let (y, mo, d, h, mi, s) = secs_to_ymd_hms(secs);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z"
    )
}

/// Civil-date breakdown for a Unix timestamp (UTC). Implements the algorithm
/// in <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
/// to avoid pulling chrono just for the audit timestamp.
fn secs_to_ymd_hms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u32;
    let h = secs_of_day / 3600;
    let mi = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y0 = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y0 + 1 } else { y0 };
    (y as i32, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_known_dates() {
        // 2026-04-27 15:32:14 UTC → 1777303934
        let (y, mo, d, h, mi, s) = secs_to_ymd_hms(1_777_303_934);
        assert_eq!((y, mo, d, h, mi, s), (2026, 4, 27, 15, 32, 14));
        // Epoch.
        let (y, mo, d, h, mi, s) = secs_to_ymd_hms(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
        // Leap day.
        let (y, mo, d, _, _, _) = secs_to_ymd_hms(951_782_400);
        assert_eq!((y, mo, d), (2000, 2, 29));
    }

    #[test]
    fn appends_jsonl_and_rotates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = Entry {
            ts: now_rfc3339(),
            kind: Kind::FsWrite,
            path: "src/Foo.luau",
            sha_before: Some("aaaa"),
            sha_after: Some("bbbb"),
            session_id: "test-session",
            note: None,
        };
        record(dir.path(), &entry);
        let logp = log_path(dir.path());
        let body = std::fs::read_to_string(&logp).expect("read");
        assert!(body.contains("\"kind\":\"fs_write\""));
        assert!(body.ends_with('\n'));

        // Force rotation by stuffing the file beyond MAX_BYTES.
        std::fs::write(&logp, vec![b'x'; (MAX_BYTES + 1) as usize]).expect("inflate");
        record(dir.path(), &entry);
        assert!(logp.with_extension("log.1").exists(), "rotation should move .log.1");
    }
}
