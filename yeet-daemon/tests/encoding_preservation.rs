//! Round-trip tests for the meta-preserving encoding path.
//!
//! `state::normalize_from_disk` strips UTF-8 BOMs and collapses CRLF →
//! LF when ingesting a file from disk, recording what was there in
//! `FileMeta` so `encode_for_disk` can reapply both on the way out.
//! Every write surface (write_to_fs, manual resolve, conflict
//! resolution, syncback) goes through that pair, so a regression that
//! drops the meta lookup would silently re-flavor user files.
//!
//! These tests pin the most production-credible path: a Windows team's
//! CRLF files survive a manual conflict resolution sourced from the
//! line-by-line merge picker (Luau strings have no CRLF concept, so
//! the picker only knows LF — the daemon must reapply CRLF on write).

mod common;

use std::time::Duration;

use anyhow::Result;
use common::{connect_as, next_matching, send_json, spawn_test_daemon};
use serde_json::json;
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Disk file uses CRLF. Manual resolve sends LF-only content (the
/// only thing the merge picker can produce — Luau strings don't carry
/// `\r`). Daemon must reapply CRLF on the way back to disk so a
/// Windows team's `git diff` doesn't suddenly show 3000 line-ending
/// changes after a sync round-trip.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_preserves_crlf_line_endings() -> Result<()> {
    // Three lines on disk in CRLF flavor.
    let on_disk_crlf = "old line 1\r\nold line 2\r\nold line 3\r\n";
    let daemon = spawn_test_daemon(&[("src/Crlf.luau", on_disk_crlf)])?;
    let abs = daemon.project_root.path().join("src/Crlf.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // The merge picker sends LF-only content because Luau strings
    // don't model CRLF — every "newline" is `\n`.
    let merged_lf = "new line 1\nnew line 2\nnew line 3\n";
    let sha = sha256_hex(merged_lf.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Crlf.luau",
            "content": merged_lf,
            "sha256": sha,
        }),
    )
    .await?;

    // Wait for the write — read raw bytes (NOT read_to_string, which
    // would normalize on some platforms in some Rust versions). The
    // file MUST contain CRLF, not LF.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let bytes = std::fs::read(&abs)?;
        let s = String::from_utf8_lossy(&bytes);
        // We're done when the file content reflects the merged lines.
        if s.contains("new line 1") {
            assert!(
                s.contains("\r\n"),
                "CRLF flavor must be preserved on write; got bytes {bytes:?}"
            );
            assert!(
                !s.contains("\n\r"),
                "Mangled line endings (\\n\\r) detected; got bytes {bytes:?}"
            );
            // Specific check: the body should be the merged lines with
            // CRLF reapplied byte-for-byte.
            let expected = "new line 1\r\nnew line 2\r\nnew line 3\r\n";
            assert_eq!(
                String::from_utf8(bytes).expect("utf8"),
                expected,
                "CRLF reapplication should be byte-identical to disk-format",
            );
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            panic!("manual resolve did not propagate within 3s; last seen {s:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Disk file starts with a UTF-8 BOM. Manual resolve sends content
/// without one (the merge picker has no concept of BOMs). Daemon's
/// encode_for_disk must restore the BOM so editors / tools that
/// detected the BOM previously still see it.
#[tokio::test(flavor = "multi_thread")]
async fn manual_resolve_preserves_utf8_bom() -> Result<()> {
    // BOM (EF BB BF in UTF-8 = U+FEFF) followed by a small body.
    let on_disk_bom = "\u{feff}-- Original BOM file\nlocal x = 1\n";
    let daemon = spawn_test_daemon(&[("src/Bom.luau", on_disk_bom)])?;
    let abs = daemon.project_root.path().join("src/Bom.luau");
    let mut plugin = connect_as(&daemon, "plugin").await?;
    let _ = next_matching(&mut plugin, Duration::from_secs(3), |v| {
        v.get("type").and_then(|t| t.as_str()) == Some("project_opened")
    })
    .await?;

    // Merged content has no BOM (Luau strings do not encode BOMs).
    let merged_no_bom = "-- merged\nlocal y = 2\n";
    let sha = sha256_hex(merged_no_bom.as_bytes());
    send_json(
        &mut plugin,
        json!({
            "type": "conflict_resolved_manual",
            "path": "src/Bom.luau",
            "content": merged_no_bom,
            "sha256": sha,
        }),
    )
    .await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let bytes = std::fs::read(&abs)?;
        // Done when the body reflects the merged content.
        if bytes.windows("-- merged".len()).any(|w| w == b"-- merged") {
            // First three bytes MUST still be the BOM.
            assert!(
                bytes.len() >= 3 && bytes[0..3] == [0xEF, 0xBB, 0xBF],
                "BOM must be reapplied on write; got first 8 bytes = {:?}",
                &bytes[..bytes.len().min(8)]
            );
            // No double-BOM.
            assert!(
                bytes[3..].windows(3).all(|w| w != [0xEF, 0xBB, 0xBF]),
                "BOM must appear exactly once at the start; got bytes {bytes:?}"
            );
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "manual resolve did not propagate within 3s; last seen {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
