use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const PDFIUM_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/", env!("PDFIUM_LIBRARY_NAME")));
const PDFIUM_REVISION: &str = env!("PDFIUM_REVISION");
const PDFIUM_LIBRARY_NAME: &str = env!("PDFIUM_LIBRARY_NAME");

pub fn materialize() -> io::Result<PathBuf> {
    materialize_in(&cache_root())
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("pdfterm")
}

fn materialize_in(cache_root: &Path) -> io::Result<PathBuf> {
    let directory = cache_root.join(format!(
        "pdfium-{PDFIUM_REVISION}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    fs::create_dir_all(&directory)?;
    let library = directory.join(PDFIUM_LIBRARY_NAME);

    if file_has_expected_size(&library)? {
        return Ok(library);
    }

    let temporary = directory.join(format!(
        ".{}.{}.tmp",
        OsStr::new(PDFIUM_LIBRARY_NAME).to_string_lossy(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(PDFIUM_BYTES)?;
    file.sync_all()?;
    drop(file);

    match fs::rename(&temporary, &library) {
        Ok(()) => Ok(library),
        Err(_error) if file_has_expected_size(&library)? => {
            let _ = fs::remove_file(temporary);
            Ok(library)
        }
        Err(error) => {
            let _ = fs::remove_file(temporary);
            Err(error)
        }
    }
}

fn file_has_expected_size(path: &Path) -> io::Result<bool> {
    match path.metadata() {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() == PDFIUM_BYTES.len() as u64),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_embedded_library_once() {
        let cache = tempfile::tempdir().unwrap();
        let first = materialize_in(cache.path()).unwrap();
        let second = materialize_in(cache.path()).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.metadata().unwrap().len(), PDFIUM_BYTES.len() as u64);
    }
}
