//! Resolution of the on-disk locations Javelin is allowed to write to.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};

/// Root directory for persistent indexes and caches.
///
/// Honours the configured override, otherwise the platform cache directory.
/// Nothing is created here; callers create subdirectories lazily so a read-only
/// home directory does not break startup.
pub fn cache_root(configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = configured {
        return Ok(dir.to_path_buf());
    }
    dirs::cache_dir()
        .map(|dir| dir.join("javelin"))
        .ok_or_else(|| Error::config("no platform cache directory; set index.cacheDir"))
}

/// Per-workspace cache directory, derived from the workspace path so two
/// projects never share an index.
pub fn workspace_cache_dir(cache_root: &Path, workspace: &Path) -> PathBuf {
    cache_root.join("workspaces").join(stable_key(workspace))
}

/// The file in a workspace's cache directory naming the workspace, rewritten
/// by every session that opens it, so its age is the cache's last use.
const ROOT_FILE: &str = "root";

/// How long a workspace's cache is kept after the last session that used it.
pub const UNUSED_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Record that a session is using `workspace`'s cache directory, and for what.
pub fn mark_workspace_cache(cache_root: &Path, workspace: &Path) -> std::io::Result<()> {
    let dir = workspace_cache_dir(cache_root, workspace);
    std::fs::create_dir_all(&dir)?;
    // A path that is not UTF-8 is not written, so it is pruned by age alone.
    std::fs::write(dir.join(ROOT_FILE), workspace.to_str().unwrap_or_default())
}

/// Delete every workspace cache but `keep`'s whose workspace is gone, or that
/// no session has used for [`UNUSED_FOR`], and answer what was deleted.
///
/// Gone is a recorded folder that is missing while its parent is not — a
/// removed git worktree, each of which is a cache of its own, and hundreds of
/// megabytes — so a volume that is not mounted keeps its cache. Only a
/// directory named like a key is touched, whatever else the root holds.
pub fn prune_workspace_caches(cache_root: &Path, keep: &Path, now: SystemTime) -> Vec<PathBuf> {
    let keep = workspace_cache_dir(cache_root, keep);
    let Ok(entries) = std::fs::read_dir(cache_root.join("workspaces")) else {
        return Vec::new();
    };
    let mut pruned = Vec::new();
    for dir in entries.flatten().map(|entry| entry.path()) {
        if dir == keep || !dir.is_dir() || !is_key(&dir) {
            continue;
        }
        let gone = std::fs::read_to_string(dir.join(ROOT_FILE))
            .ok()
            .filter(|root| !root.is_empty())
            .map(PathBuf::from)
            .is_some_and(|root| !root.exists() && root.parent().is_some_and(Path::is_dir));
        let unused = last_used(&dir)
            .and_then(|used| now.duration_since(used).ok())
            .is_some_and(|idle| idle > UNUSED_FOR);
        // Two sessions may prune at once; the one that loses has nothing to do.
        if (gone || unused) && std::fs::remove_dir_all(&dir).is_ok() {
            pruned.push(dir);
        }
    }
    pruned
}

/// Whether a directory's name is one [`stable_key`] writes.
fn is_key(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once('-'))
        .is_some_and(|(_, hash)| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The newest modification time of anything directly in `dir`: a session
/// writes its index, its classpath or its marker there.
fn last_used(dir: &Path) -> Option<SystemTime> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()
}

/// A filesystem-safe, stable key for a path.
///
/// ponytail: FNV-1a is enough to keep directory names short and collision-free
/// in practice; swap for a real digest if workspace keys ever become security
/// relevant.
pub fn stable_key(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let sanitized: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    format!("{sanitized}-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_cache_dir_wins() {
        let root = cache_root(Some(Path::new("/tmp/jv")));
        assert_eq!(root.as_deref().ok(), Some(Path::new("/tmp/jv")));
    }

    #[test]
    fn different_workspaces_get_different_directories() {
        let root = Path::new("/cache");
        let a = workspace_cache_dir(root, Path::new("/home/u/projects/app"));
        let b = workspace_cache_dir(root, Path::new("/home/u/other/app"));
        assert_ne!(a, b, "same basename must not collide across paths");
        assert_eq!(
            a,
            workspace_cache_dir(root, Path::new("/home/u/projects/app"))
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("javelin-prune-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn age(dir: &Path, by: Duration) {
        let then = SystemTime::now() - by;
        for entry in std::fs::read_dir(dir).unwrap() {
            let file = std::fs::File::open(entry.unwrap().path()).unwrap();
            file.set_modified(then).unwrap();
        }
    }

    #[test]
    fn a_cache_whose_workspace_is_gone_or_unused_is_pruned_and_no_other() {
        let base = scratch("cases");
        let cache = base.join("cache");
        let projects = base.join("projects");
        let workspace = |name: &str| {
            let root = projects.join(name);
            std::fs::create_dir_all(&root).unwrap();
            mark_workspace_cache(&cache, &root).unwrap();
            std::fs::write(
                workspace_cache_dir(&cache, &root).join("sources.sqlite"),
                b"x",
            )
            .unwrap();
            root
        };
        let current = workspace("current");
        let other = workspace("other");
        // A removed git worktree: its folder is gone, its parent is not.
        let removed = workspace("removed");
        std::fs::remove_dir_all(&removed).unwrap();
        // On a volume that is not mounted: nothing above it exists either.
        let unmounted = base.join("Volumes/usb/app");
        mark_workspace_cache(&cache, &unmounted).unwrap();
        // Not opened for longer than the limit, the session's own included.
        let idle = workspace("idle");
        age(
            &workspace_cache_dir(&cache, &idle),
            UNUSED_FOR + Duration::from_secs(60),
        );
        age(&workspace_cache_dir(&cache, &current), UNUSED_FOR * 2);
        // Written before the marker existed, and used last week.
        let unmarked = cache.join("workspaces").join("legacy-0123456789abcdef");
        std::fs::create_dir_all(&unmarked).unwrap();
        std::fs::write(unmarked.join("sources.sqlite"), b"x").unwrap();
        // Something that is not a workspace cache at all.
        let stranger = cache.join("workspaces").join("notes");
        std::fs::create_dir_all(&stranger).unwrap();
        std::fs::write(stranger.join("todo.txt"), b"x").unwrap();
        age(&stranger, UNUSED_FOR * 2);

        let mut pruned = prune_workspace_caches(&cache, &current, SystemTime::now());
        pruned.sort();
        let mut expected = vec![
            workspace_cache_dir(&cache, &removed),
            workspace_cache_dir(&cache, &idle),
        ];
        expected.sort();
        assert_eq!(pruned, expected);
        for kept in [&current, &other, &unmounted] {
            assert!(
                workspace_cache_dir(&cache, kept).is_dir(),
                "{}",
                kept.display()
            );
        }
        assert!(unmarked.is_dir() && stranger.is_dir());
        for gone in &expected {
            assert!(!gone.exists(), "{}", gone.display());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn keys_stay_filesystem_safe() {
        let dir = workspace_cache_dir(Path::new("/cache"), Path::new("/home/u/my project (v2)"));
        let leaf = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(
            leaf.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{leaf}"
        );
    }
}
