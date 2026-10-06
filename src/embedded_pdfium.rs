use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const PDFIUM_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/", env!("PDFIUM_LIBRARY_NAME")));
const PDFIUM_REVISION: &str = env!("PDFIUM_REVISION");
const PDFIUM_LIBRARY_NAME: &str = env!("PDFIUM_LIBRARY_NAME");

pub fn materialize() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| {
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
    let temporary = directory.join(format!(".{PDFIUM_LIBRARY_NAME}.{}.tmp", std::process::id()));
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
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.len() != PDFIUM_BYTES.len() as u64
    {
        return Err(io::Error::other(format!(
            "unsafe PDFium cache file: {}",
            path.display()
        )));
    }
    let mut buffer = [0; 65536];
    for expected in PDFIUM_BYTES.chunks(buffer.len()) {
        file.read_exact(&mut buffer[..expected.len()])?;
        if &buffer[..expected.len()] != expected {
            return Err(io::Error::other(format!(
                "PDFium cache content mismatch: {}",
                path.display()
            )));
        }
    }
    if file.read(&mut buffer[..1])? != 0 {
        return Err(io::Error::other(
            "PDFium cache changed while being verified",
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::symlink;

    #[test]
    fn reuses_verified_library_but_rejects_same_size_tampering() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("private");
        let first = materialize_in(&cache).unwrap();
        let identity = first.metadata().unwrap().ino();
        assert_eq!(
            materialize_in(&cache).unwrap().metadata().unwrap().ino(),
            identity
        );
        let mut file = OpenOptions::new().write(true).open(&first).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[PDFIUM_BYTES[0] ^ 0xff]).unwrap();
        assert!(
            materialize_in(&cache)
                .unwrap_err()
                .to_string()
                .contains("content mismatch")
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
