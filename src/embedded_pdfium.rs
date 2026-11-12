use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PDFIUM_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/", env!("PDFIUM_LIBRARY_NAME")));
const PDFIUM_REVISION: &str = env!("PDFIUM_REVISION");
const PDFIUM_LIBRARY_NAME: &str = env!("PDFIUM_LIBRARY_NAME");
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn materialize() -> io::Result<PathBuf> {
    let base = crate::config::xdg_dir("XDG_CACHE_HOME", ".cache").ok_or_else(|| {
        io::Error::other("HOME or XDG_CACHE_HOME must be set for the PDFium cache")
    })?;
    fs::create_dir_all(&base)?;
    materialize_in(&base.join("pdfterm-private"))
}

fn materialize_in(cache_root: &Path) -> io::Result<PathBuf> {
    crate::ipc::private_dir(cache_root)?;
    let directory = cache_root.join(format!(
        "pdfium-{PDFIUM_REVISION}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    crate::ipc::private_dir(&directory)?;
    let library = directory.join(PDFIUM_LIBRARY_NAME);
    if verified_file(&library)? {
        return Ok(library);
    }
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".{PDFIUM_LIBRARY_NAME}.{}.{id}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let installed = (|| {
        file.write_all(PDFIUM_BYTES)?;
        file.sync_all()?;
        // Unlike rename, hard_link cannot overwrite another process's result.
        match fs::hard_link(&temporary, &library) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if verified_file(&library)? {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    })();
    drop(file);
    let cleanup = fs::remove_file(&temporary);
    installed?;
    cleanup?;
    Ok(library)
}

fn verified_file(path: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_file()
        || !crate::ipc::is_owned_with_mask(&metadata, 0o022)
        || metadata.len() != PDFIUM_BYTES.len() as u64
    {
        return Err(io::Error::other(format!(
            "unsafe PDFium cache file: {}",
            path.display()
        )));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[test]
    fn reuses_verified_library_and_rejects_unsafe_metadata() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("private");
        let first = materialize_in(&cache).unwrap();
        let identity = first.metadata().unwrap().ino();
        assert_eq!(
            materialize_in(&cache).unwrap().metadata().unwrap().ino(),
            identity
        );

        fs::set_permissions(&first, fs::Permissions::from_mode(0o620)).unwrap();
        assert!(
            materialize_in(&cache)
                .unwrap_err()
                .to_string()
                .contains("unsafe PDFium cache file")
        );
        fs::set_permissions(&first, fs::Permissions::from_mode(0o600)).unwrap();

        let mut file = OpenOptions::new().write(true).open(&first).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(&[0]).unwrap();
        drop(file);
        assert!(
            materialize_in(&cache)
                .unwrap_err()
                .to_string()
                .contains("unsafe PDFium cache file")
        );
    }

    #[test]
    fn rejects_symlink_library_without_modifying_target() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("private");
        let library = materialize_in(&cache).unwrap();
        fs::remove_file(&library).unwrap();
        let target = root.path().join("unrelated");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, &library).unwrap();
        assert!(materialize_in(&cache).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }
}
