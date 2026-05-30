use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use parking_lot::Mutex;

use crate::error::NpxcError;

/// `errno` for a cross-device link (`EXDEV`) on Unix. Hard links fail with this
/// when the source and destination live on different filesystems, in which case
/// we fall back to a copy. The value is `18` on Linux and macOS (the only
/// platforms `npxc` targets).
const EXDEV: i32 = 18;

/// Cache key: the combination of canonical host path and file mtime (nanoseconds
/// since UNIX epoch) uniquely identifies an unmodified file version.
pub type PublicationKey = (PathBuf, u64);

/// Metadata for a file that has been published into the session workspace.
#[derive(Debug, Clone)]
pub struct PublishedFile {
    /// Random UUID used as the per-file subdirectory name.
    pub uuid: String,
    /// Original filename (last path component).
    pub basename: String,
    /// Absolute path inside the container: `/workspace/<uuid>/<basename>`.
    pub container_path: String,
    /// Host-side directory created for this publication: `<session_dir>/<uuid>/`.
    pub host_dir: PathBuf,
    /// Canonical host path of the source file.
    pub host_path: PathBuf,
}

/// In-memory registry of every file published in the current session.
#[derive(Debug)]
pub struct PublicationCache {
    /// Forward map: (`canonical_path`, `mtime_nanos`) → published file metadata.
    entries: HashMap<PublicationKey, PublishedFile>,
    /// Reverse map: `container_path` → canonical host path.
    /// Used by the response pipeline to rewrite `/workspace/…` paths back to
    /// their host equivalents before forwarding output to the client.
    reverse: HashMap<String, PathBuf>,
}

impl Default for PublicationCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicationCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    /// Look up a published file by canonical path + mtime without mutating the
    /// cache. Returns `None` when the file has not been published (or has
    /// changed on disk since the last publication).
    #[must_use]
    pub fn get(&self, canonical_path: &Path, mtime_nanos: u64) -> Option<&PublishedFile> {
        self.entries
            .get(&(canonical_path.to_path_buf(), mtime_nanos))
    }

    /// Record a newly published file in both the forward and reverse maps.
    pub fn insert(&mut self, key: PublicationKey, file: PublishedFile) {
        self.reverse
            .insert(file.container_path.clone(), file.host_path.clone());
        self.entries.insert(key, file);
    }

    /// Return a point-in-time snapshot of the reverse map as owned
    /// `(container_path, host_path_string)` pairs.
    ///
    /// The caller can use these pairs for string replacement in responses
    /// without needing to hold the cache lock across any I/O or `.await` points.
    #[must_use]
    pub fn reverse_snapshot(&self) -> Vec<(String, String)> {
        self.reverse
            .iter()
            .map(|(container, host)| (container.clone(), host.to_string_lossy().into_owned()))
            .collect()
    }
}

/// Publish `canonical_path` into the session workspace directory, returning
/// the container-side path `/workspace/<uuid>/<basename>`.
///
/// # Deduplication
///
/// Files are keyed by `(canonical_path, mtime_nanos)`. A second call for the
/// same unmodified file returns the already-published container path without
/// touching the filesystem again.
///
/// # Cross-filesystem fallback
///
/// A hard link is attempted first. On `EXDEV` (errno 18, cross-filesystem
/// mount), the file is copied instead via `tokio::task::spawn_blocking`.
///
/// # Race-condition safety
///
/// The cache mutex is **never** held across an `.await` point. After the I/O
/// completes we re-acquire the lock and check whether a concurrent task already
/// published the same key. If so, our newly created directory is removed and
/// the winner's container path is returned.
///
/// # Errors
///
/// Returns [`NpxcError::Io`] if the source metadata cannot be read or the
/// per-file directory, hard link, or cross-filesystem copy fails, and
/// [`NpxcError::Runtime`] if the blocking copy task panics or is cancelled.
pub async fn publish_file(
    canonical_path: &Path,
    session_dir: &Path,
    cache: &Mutex<PublicationCache>,
) -> Result<String, NpxcError> {
    // ── Step 1: read mtime from metadata ────────────────────────────────────
    let metadata = std::fs::metadata(canonical_path)?;
    let modified = metadata.modified()?;
    let mtime_nanos = modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));

    let key: PublicationKey = (canonical_path.to_path_buf(), mtime_nanos);

    // ── Step 2: check cache (lock → check → unlock, no await inside) ────────
    {
        let guard = cache.lock();
        if let Some(published) = guard.get(canonical_path, mtime_nanos) {
            return Ok(published.container_path.clone());
        }
        // Lock is released here before any I/O.
    }

    // ── Step 3: perform I/O with the lock released ──────────────────────────
    let uuid = uuid::Uuid::new_v4().to_string();
    let basename = canonical_path
        .file_name()
        .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());

    let host_dir = session_dir.join(&uuid);
    let dest = host_dir.join(&basename);
    let container_path = format!("/workspace/{uuid}/{basename}");

    tokio::fs::create_dir_all(&host_dir).await?;

    // Try a hard link first; fall back to a blocking copy on EXDEV.
    if let Err(link_err) = tokio::fs::hard_link(canonical_path, &dest).await {
        if link_err.raw_os_error() == Some(EXDEV) {
            // Source and destination are on different filesystems.
            let copy_src = canonical_path.to_path_buf();
            let copy_dst = dest.clone();
            tokio::task::spawn_blocking(move || std::fs::copy(&copy_src, &copy_dst))
                .await
                .map_err(|e| NpxcError::Runtime(e.to_string()))??;
        } else {
            return Err(link_err.into());
        }
    }

    // ── Step 4: re-lock, race-check, then insert or clean up ────────────────
    let published = PublishedFile {
        uuid,
        basename,
        container_path: container_path.clone(),
        host_dir: host_dir.clone(),
        host_path: canonical_path.to_path_buf(),
    };

    let racing_winner: Option<String> = {
        let mut guard = cache.lock();
        if let Some(existing) = guard.get(canonical_path, mtime_nanos) {
            // A concurrent task finished first — return its result.
            Some(existing.container_path.clone())
        } else {
            guard.insert(key, published);
            None
        }
        // Lock is released here.
    };

    if let Some(winner_path) = racing_winner {
        // Best-effort cleanup of the directory we just created unnecessarily.
        let _ = tokio::fs::remove_dir_all(&host_dir).await;
        return Ok(winner_path);
    }

    Ok(container_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn cache_get_miss() {
        let cache = PublicationCache::new();
        assert!(cache.get(Path::new("/no/such"), 0).is_none());
    }

    #[test]
    fn cache_insert_and_get() {
        let mut cache = PublicationCache::new();
        let path = PathBuf::from("/host/file.txt");
        let key = (path.clone(), 12345_u64);
        let file = PublishedFile {
            uuid: "u1".into(),
            basename: "file.txt".into(),
            container_path: "/workspace/u1/file.txt".into(),
            host_dir: PathBuf::from("/session/u1"),
            host_path: path.clone(),
        };
        cache.insert(key, file);

        let hit = cache.get(&path, 12345).unwrap();
        assert_eq!(hit.container_path, "/workspace/u1/file.txt");
    }

    #[test]
    fn cache_miss_on_stale_mtime() {
        let mut cache = PublicationCache::new();
        let path = PathBuf::from("/host/file.txt");
        let key = (path.clone(), 100_u64);
        let file = PublishedFile {
            uuid: "u1".into(),
            basename: "file.txt".into(),
            container_path: "/workspace/u1/file.txt".into(),
            host_dir: PathBuf::from("/session/u1"),
            host_path: path.clone(),
        };
        cache.insert(key, file);
        // Different mtime → cache miss
        assert!(cache.get(&path, 200).is_none());
    }

    #[test]
    fn reverse_snapshot_contents() {
        let mut cache = PublicationCache::new();
        let path = PathBuf::from("/host/report.pdf");
        let key = (path.clone(), 42_u64);
        let file = PublishedFile {
            uuid: "abc".into(),
            basename: "report.pdf".into(),
            container_path: "/workspace/abc/report.pdf".into(),
            host_dir: PathBuf::from("/session/abc"),
            host_path: path.clone(),
        };
        cache.insert(key, file);

        let snap = cache.reverse_snapshot();
        assert_eq!(snap.len(), 1);
        let (cont, host) = &snap[0];
        assert_eq!(cont, "/workspace/abc/report.pdf");
        assert_eq!(host, "/host/report.pdf");
    }

    #[tokio::test]
    async fn publish_file_hardlinks_and_deduplicates() {
        let src_dir = tmp();
        let session_dir = tmp();

        let src_file = src_dir.path().join("data.bin");
        std::fs::write(&src_file, b"hello").unwrap();
        let canonical = std::fs::canonicalize(&src_file).unwrap();

        let cache: Arc<Mutex<PublicationCache>> = Arc::new(Mutex::new(PublicationCache::new()));

        let path1 = publish_file(&canonical, session_dir.path(), &cache)
            .await
            .unwrap();
        assert!(path1.starts_with("/workspace/"));

        // Second call → same container path (deduplication).
        let path2 = publish_file(&canonical, session_dir.path(), &cache)
            .await
            .unwrap();
        assert_eq!(path1, path2);
    }
}
