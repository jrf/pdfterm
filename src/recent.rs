use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const MAX_RECENTS: usize = 50;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Loads the recently opened documents, most-recent first. Returns an empty
/// list when no history file exists or it cannot be read.
pub fn load() -> Vec<PathBuf> {
    let Some(path) = recent_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .take(MAX_RECENTS)
        .map(PathBuf::from)
        .collect()
}

/// Records `path` as the most recently opened document, promoting it to the
/// front of the history and capping the list. Failures are ignored so history
/// never blocks opening a document.
pub fn record(path: &Path) {
    let Some(store) = recent_path() else {
        return;
    };
    record_to(&store, path);
}

fn record_to(store: &Path, path: &Path) {
    if path
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'\n' | b'\r'))
    {
        return;
    }
    let existing = fs::read_to_string(store)
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let entries = promote(existing, path, MAX_RECENTS);
    let Some(parent) = store.parent() else {
        return;
    };
    if let Some(grandparent) = parent.parent()
        && fs::create_dir_all(grandparent).is_err()
    {
        return;
    }
    match DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return,
    }
    let Ok(dir) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
    else {
        return;
    };
    let Ok(meta) = dir.metadata() else {
        return;
    };
    if !crate::ipc::is_owned_with_mask(&meta, 0)
        || dir
            .set_permissions(fs::Permissions::from_mode(0o700))
            .is_err()
    {
        return;
    }
    let contents = entries
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".recent.{}.{id}.tmp", std::process::id()));
    let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
    else {
        return;
    };
    if file.write_all(contents.as_bytes()).is_ok() {
        drop(file);
        let _ = fs::rename(&temp, store);
    } else {
        drop(file);
    }
    let _ = fs::remove_file(&temp);
}

/// Moves `path` to the front of `entries`, removing any earlier occurrence and
/// truncating to `max` items.
fn promote(mut entries: Vec<PathBuf>, path: &Path, max: usize) -> Vec<PathBuf> {
    entries.retain(|existing| existing != path);
    entries.insert(0, path.to_path_buf());
    entries.truncate(max);
    entries
}

fn recent_path() -> Option<PathBuf> {
    Some(crate::config::xdg_dir("XDG_CACHE_HOME", ".cache")?.join("pdfterm/recent"))
}

#[cfg(test)]
mod tests {
    use super::promote;
    use std::path::PathBuf;

    #[test]
    fn promote_moves_existing_entry_to_front() {
        let entries = vec![PathBuf::from("/a.pdf"), PathBuf::from("/b.pdf")];

        let promoted = promote(entries, &PathBuf::from("/b.pdf"), 50);

        assert_eq!(
            promoted,
            vec![PathBuf::from("/b.pdf"), PathBuf::from("/a.pdf")]
        );
    }

    #[test]
    fn promote_caps_the_history_length() {
        let entries = vec![PathBuf::from("/a.pdf"), PathBuf::from("/b.pdf")];

        let promoted = promote(entries, &PathBuf::from("/c.pdf"), 2);

        assert_eq!(
            promoted,
            vec![PathBuf::from("/c.pdf"), PathBuf::from("/a.pdf")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn record_to_creates_private_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("sub/recent");
        super::record_to(&store, std::path::Path::new("/synth/zibble.pdf"));
        assert_eq!(
            std::fs::metadata(store.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&shared, &linked).unwrap();
        super::record_to(
            &linked.join("recent"),
            std::path::Path::new("/synth/zibble.pdf"),
        );
        assert_eq!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!shared.join("recent").exists());
    }
}
