//! Generic single-file change watcher.
//!
//! Thin wrapper over [`notify`] that fires a callback whenever a specific file
//! changes on disk. Used by the binary to hot-reload the static token file,
//! but knows nothing about tokens — it's a reusable building block (skills /
//! registry hot-reload could use it too).
//!
//! ## Why it watches the PARENT directory, not the file
//!
//! Callers write the watched file atomically: write `<file>.tmp`, then
//! `rename` it over the target. On Linux that `rename` swaps the inode — a
//! watch registered on the file's inode goes stale after the first swap and
//! never fires again. Watching the *parent directory* (and filtering events to
//! the target file name) survives inode replacement, which is exactly the
//! semantic we need for atomic-write hot-reload.
//!
//! ## Threading
//!
//! `notify` runs the event handler on its own background thread, NOT the tokio
//! runtime. The callback you pass must therefore be `Send + 'static` and must
//! only do synchronous work (read the file, parse, swap an `ArcSwap`). Do not
//! call async code or `block_on` from inside it.

use std::path::{Path, PathBuf};

use notify::{recommended_watcher, Event, RecommendedWatcher, RecursiveMode, Watcher};

/// Re-export so callers can name the watcher guard type without a direct
/// `notify` dependency.
pub use notify::RecommendedWatcher as FileWatcher;

/// Start watching `path` for changes; invoke `cb` on every change that touches
/// it. The returned [`RecommendedWatcher`] MUST be kept alive for as long as
/// you want events — dropping it silently stops the watch. Bind it like
/// `let _watcher = spawn_file_watcher(...)?;` for the process lifetime.
///
/// The watch is registered on the parent directory (non-recursive) and events
/// are filtered to `path`'s file name, so it survives the tmp+rename atomic
/// write pattern (see module docs).
pub fn spawn_file_watcher<F>(path: &Path, cb: F) -> anyhow::Result<RecommendedWatcher>
where
    F: Fn() + Send + 'static,
{
    let target_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("watch path has no file name: {}", path.display()))?
        .to_owned();

    // An empty parent (e.g. bare "tokens.json") means the current directory.
    let parent: PathBuf = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };

    let filter_name = target_name.clone();
    let mut watcher = recommended_watcher(move |res: Result<Event, notify::Error>| match res {
        Ok(event) => {
            let touches_target = event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(filter_name.as_os_str()));
            if touches_target {
                tracing::debug!(kind = ?event.kind, "watched file changed");
                cb();
            }
        }
        Err(e) => tracing::warn!(error = %e, "file watcher error"),
    })?;

    watcher.watch(&parent, RecursiveMode::NonRecursive)?;
    tracing::info!(dir = %parent.display(), file = ?target_name, "file watcher started");
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// RAII temp dir without pulling in `tempfile`.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("vmcp-watch-test-{nanos}"));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Mimic the production atomic write: write `<path>.tmp`, rename over target.
    fn atomic_write(path: &Path, contents: &[u8]) {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, contents).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    /// Block until `count` exceeds `baseline` or the timeout elapses; return the
    /// final observed value.
    fn wait_until_increased(count: &AtomicUsize, baseline: usize, max: Duration) -> usize {
        let start = Instant::now();
        loop {
            let now = count.load(Ordering::SeqCst);
            if now > baseline {
                return now;
            }
            if start.elapsed() >= max {
                return now;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn callback_fires_and_survives_atomic_rename() {
        let dir = TempDir::new();
        let target = dir.path().join("tokens.json");
        atomic_write(&target, b"[]"); // initial

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _watcher = spawn_file_watcher(&target, move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("watcher starts");

        // Give the OS watch a moment to arm before the first mutation.
        std::thread::sleep(Duration::from_millis(300));

        atomic_write(&target, b"[1]");
        let after_first = wait_until_increased(&count, 0, Duration::from_secs(5));
        assert!(
            after_first > 0,
            "callback must fire after the first atomic rename"
        );

        // The crux: after the first rename swapped the inode, a file-level watch
        // would be dead. A parent-dir watch must still fire on the second write.
        atomic_write(&target, b"[1,2]");
        let after_second = wait_until_increased(&count, after_first, Duration::from_secs(5));
        assert!(
            after_second > after_first,
            "callback must fire again after a second atomic rename (parent-dir watch survives inode swap)"
        );
    }
}
