use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

const DEBOUNCE: Duration = Duration::from_millis(100);
const TICK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum FileEvent {
    /// Something under the watched root changed, was created, or was renamed
    /// *to* this path. The daemon inspects the file on disk to decide whether
    /// to emit a `FileCreated` or `FileChanged` over the wire.
    Touched(PathBuf),
    /// The path disappeared (remove or rename-away).
    Removed(PathBuf),
}

// TODO: when notify reports a directory removal (EventKind::Remove of a
// dir), call `prune_empty_dirs` on its parent so manual `rm -r` from the
// IDE side keeps the on-disk tree consistent without waiting for a bulk
// resolution. Skipped for now because notify's rename+remove burst on
// some platforms makes false-positive churn during normal user file
// moves; revisit once we have integration coverage for the rename case.

/// Spawns the notify backend and a dedicated debounce thread. Debounce is
/// path-keyed: repeated events on the same path are coalesced into the most
/// recent kind, and flushed 100 ms after the last activity. This is resilient
/// to editors that save by write-to-temp + rename, which produces a flurry of
/// Remove/Create/Modify within a few milliseconds.
///
/// The returned `Watcher` MUST be kept alive — dropping it stops the underlying
/// platform backend.
pub fn spawn(root: &Path, tx: mpsc::Sender<FileEvent>) -> Result<RecommendedWatcher> {
    let (raw_tx, raw_rx) = std_mpsc::channel::<notify::Result<notify::Event>>();

    let mut watcher = notify::recommended_watcher(move |res| {
        // Best-effort; if the debounce thread is gone, we're shutting down anyway.
        let _ = raw_tx.send(res);
    })
    .context("create file watcher")?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("watch {}", root.display()))?;

    std::thread::Builder::new()
        .name("yeet-watcher-debounce".into())
        .spawn(move || debounce_loop(raw_rx, tx))
        .context("spawn debounce thread")?;

    Ok(watcher)
}

// The receiver and sender are moved into the spawned thread and live for the
// thread's entire lifetime, so owning them here is the natural shape.
#[allow(clippy::needless_pass_by_value)]
fn debounce_loop(
    raw_rx: std_mpsc::Receiver<notify::Result<notify::Event>>,
    tx: mpsc::Sender<FileEvent>,
) {
    // Per-path: (latest kind we observed, when we last saw activity).
    let mut pending: HashMap<PathBuf, (PendingKind, Instant)> = HashMap::new();

    loop {
        match raw_rx.recv_timeout(TICK) {
            Ok(Ok(event)) => {
                let kind = PendingKind::from(&event.kind);
                let now = Instant::now();
                for path in event.paths {
                    pending.insert(path, (kind, now));
                }
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "notify error");
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }

        let now = Instant::now();
        let ready: Vec<(PathBuf, PendingKind)> = pending
            .iter()
            .filter(|(_, (_, ts))| now.duration_since(*ts) >= DEBOUNCE)
            .map(|(p, (k, _))| (p.clone(), *k))
            .collect();

        for (path, kind) in ready {
            pending.remove(&path);
            let outbound = match kind {
                PendingKind::Touched => FileEvent::Touched(path),
                PendingKind::Removed => FileEvent::Removed(path),
                PendingKind::Ignored => continue,
            };
            if tx.blocking_send(outbound).is_err() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PendingKind {
    Touched,
    Removed,
    Ignored,
}

impl From<&EventKind> for PendingKind {
    fn from(kind: &EventKind) -> Self {
        match kind {
            EventKind::Create(_) | EventKind::Modify(_) => Self::Touched,
            EventKind::Remove(_) => Self::Removed,
            // Access events and raw "Any" on some platforms are uninteresting;
            // we re-poll disk when something actionable happens.
            _ => Self::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Watcher integration: spins up a real `notify` backend on a temp
    //! directory and asserts the debounced event stream that downstream
    //! `handle_fs_event` consumes.

    use super::{spawn, FileEvent, PendingKind};
    use notify::EventKind;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn pending_kind_classifies_event_kinds() {
        // Create / Modify both surface as "the file is interesting now,
        // re-read it" — the daemon doesn't care which.
        assert!(matches!(
            PendingKind::from(&EventKind::Create(notify::event::CreateKind::File)),
            PendingKind::Touched
        ));
        assert!(matches!(
            PendingKind::from(&EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            ))),
            PendingKind::Touched
        ));
        assert!(matches!(
            PendingKind::from(&EventKind::Remove(notify::event::RemoveKind::File)),
            PendingKind::Removed
        ));
        // Access / Other — debouncer drops these so the daemon doesn't get
        // woken up for stat() / chmod() noise.
        assert!(matches!(
            PendingKind::from(&EventKind::Access(notify::event::AccessKind::Read)),
            PendingKind::Ignored
        ));
        assert!(matches!(
            PendingKind::from(&EventKind::Other),
            PendingKind::Ignored
        ));
    }

    /// Spins up the watcher against a fresh tempdir and returns the tempdir
    /// (to keep it alive) + the receiver the daemon would normally consume.
    fn spawn_watcher() -> (tempfile::TempDir, mpsc::Receiver<FileEvent>, notify::RecommendedWatcher)
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel::<FileEvent>(64);
        let w = spawn(dir.path(), tx).expect("spawn watcher");
        (dir, rx, w)
    }

    /// Wait up to `timeout` for an event, returning `None` on timeout.
    /// Tests use this instead of `recv().await` so a regression doesn't
    /// hang forever — they fail with a clear error instead.
    async fn next_event(
        rx: &mut mpsc::Receiver<FileEvent>,
        timeout: Duration,
    ) -> Option<FileEvent> {
        tokio::time::timeout(timeout, rx.recv()).await.ok().flatten()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn touched_emitted_when_file_appears() {
        let (dir, mut rx, _w) = spawn_watcher();
        let path = dir.path().join("hello.luau");
        std::fs::write(&path, b"return {}").expect("write");
        // Debounce is 100ms; allow enough time for create + flush + send.
        let evt = next_event(&mut rx, Duration::from_millis(800))
            .await
            .expect("expected a Touched event");
        match evt {
            FileEvent::Touched(p) => assert!(p.ends_with("hello.luau"), "got {p:?}"),
            other => panic!("expected Touched, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn debounce_coalesces_burst_writes_into_single_event() {
        let (dir, mut rx, _w) = spawn_watcher();
        let path = dir.path().join("burst.luau");
        for i in 0..10 {
            std::fs::write(&path, format!("write {i}").as_bytes()).expect("write");
            // Stay well inside the 100ms debounce window so the writes
            // accumulate as one logical edit.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // First event should arrive after the debounce flush.
        let _first = next_event(&mut rx, Duration::from_millis(800))
            .await
            .expect("expected at least one Touched event after the burst");
        // No further events should fire from the same burst — anything
        // else would mean the debounce didn't coalesce.
        let extra = next_event(&mut rx, Duration::from_millis(250)).await;
        assert!(
            extra.is_none(),
            "expected coalesced single event, got extra {extra:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_emits_removed_event() {
        let (dir, mut rx, _w) = spawn_watcher();
        let path = dir.path().join("ephemeral.luau");
        std::fs::write(&path, b"x").expect("write");
        // Drain the create event so the test isn't asserting about it.
        let _ = next_event(&mut rx, Duration::from_millis(800)).await;
        std::fs::remove_file(&path).expect("rm");
        let evt = next_event(&mut rx, Duration::from_millis(800))
            .await
            .expect("expected Removed event");
        match evt {
            FileEvent::Removed(p) => assert!(p.ends_with("ephemeral.luau"), "got {p:?}"),
            other => panic!("expected Removed, got {other:?}"),
        }
    }
}
