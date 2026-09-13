//! The snapshot the next start paints from, before `kubectl` has answered.
//!
//! Pods only: names, statuses, images, owners. Never a log line, never a
//! configmap's data, never a secret's — the types those live in are not in
//! this file and cannot be named from it. A pod list still says a good deal
//! about a system, so the file is written `0600`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Scope;
use crate::kube::Pod;
use crate::timestamp::Timestamp;

/// The schema this build writes. A file of any other version is ignored
/// rather than migrated: it is a cache, and the next read rewrites it.
const VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    version: u32,
    pub scopes: Vec<CachedScope>,
}

/// One scope's last read.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedScope {
    pub scope: Scope,
    pub read_at: Timestamp,
    pub pods: Vec<Pod>,
}

impl Snapshot {
    #[must_use]
    pub fn new(scopes: Vec<CachedScope>) -> Self {
        Self {
            version: VERSION,
            scopes,
        }
    }
}

/// The snapshot at `path`, or nothing at all. A file this build cannot read
/// — another version, half-written, hand-edited — is not an error worth
/// showing anyone: the read already running behind the first frame replaces
/// it.
#[must_use]
pub fn load(path: &Path) -> Option<Snapshot> {
    let source = std::fs::read_to_string(path).ok()?;
    let snapshot: Snapshot = serde_json::from_str(&source).ok()?;
    (snapshot.version == VERSION).then_some(snapshot)
}

/// Writes the snapshot atomically: a temporary file beside the real one, then
/// a rename. A start that races a save reads one file or the other, never
/// half of one.
pub fn save(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to make {}", directory.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("failed to write in {}", directory.display()))?;
    // Serialised whole and written once: a bare temp file is unbuffered.
    let bytes = serde_json::to_vec(snapshot).context("failed to write the cache")?;
    file.write_all(&bytes)
        .context("failed to write the cache")?;
    file.flush().context("failed to write the cache")?;
    restrict(file.as_file())?;
    file.persist(path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(file: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("failed to restrict the cache file")
}

#[cfg(not(unix))]
fn restrict(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::tests::{pod, scope};

    fn snapshot() -> Snapshot {
        Snapshot::new(vec![CachedScope {
            scope: scope("qa", Some("dev")),
            read_at: Timestamp::parse("2026-09-12T20:00:00Z").unwrap(),
            pods: vec![pod("qa", "dev", "orders-api-7d9f5b-abc12", "Running")],
        }])
    }

    #[test]
    fn a_snapshot_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cache.json");
        save(&path, &snapshot()).unwrap();

        let read = load(&path).unwrap();
        assert_eq!(read.scopes[0].scope, scope("qa", Some("dev")));
        assert_eq!(read.scopes[0].read_at.to_rfc3339(), "2026-09-12T20:00:00Z");
        assert_eq!(read.scopes[0].pods[0].key.name, "orders-api-7d9f5b-abc12");
    }

    #[test]
    fn a_file_this_build_cannot_read_is_simply_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        assert!(load(&path).is_none(), "a missing file");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path).is_none(), "a half-written file");

        let mut written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&snapshot()).unwrap()).unwrap();
        written["version"] = serde_json::json!(2);
        std::fs::write(&path, written.to_string()).unwrap();
        assert!(load(&path).is_none(), "a version this build does not know");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_only_by_the_user_who_wrote_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        save(&path, &snapshot()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
