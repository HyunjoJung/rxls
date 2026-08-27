//! Retained-package serialization and atomic filesystem persistence.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

use super::Spreadsheet;

pub(super) static SAVE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Spreadsheet {
    /// Save the retained OOXML package.
    pub fn save(&self) -> Result<Vec<u8>> {
        match &self.package {
            Some(package) => package.to_bytes(),
            None => Err(Error::Zip(
                "spreadsheet is read-only for package-preserving save",
            )),
        }
    }

    /// Persist the retained package through a sibling temporary file.
    ///
    /// The complete candidate bytes are serialized before the destination is
    /// touched. A uniquely created sibling file is then written, flushed with
    /// `fsync`, and atomically renamed over `path`; every pre-rename failure
    /// removes the temporary file and leaves an existing destination intact.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = self.save()?;
        atomic_write_sibling(path.as_ref(), &bytes)
    }
}

fn atomic_write_sibling(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or(Error::Zip("atomic save destination has no file name"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut opened: Option<(PathBuf, File)> = None;
    for _ in 0..128 {
        let ordinal = SAVE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".rxls-tmp-{}-{ordinal}", std::process::id()));
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                opened = Some((temp_path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(Error::Zip("failed to create atomic save temporary file")),
        }
    }
    let (temp_path, mut temp_file) = opened.ok_or(Error::Zip(
        "could not allocate a unique atomic save temporary file",
    ))?;

    if temp_file
        .write_all(bytes)
        .and_then(|_| temp_file.sync_all())
        .is_err()
    {
        close_atomic_file(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Zip("failed to write atomic save temporary file"));
    }
    close_atomic_file(temp_file);

    if fs::rename(&temp_path, path).is_err() {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::Zip("failed to atomically replace spreadsheet file"));
    }

    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| Error::Zip("failed to sync atomic save directory"))?;
    }
    Ok(())
}

fn close_atomic_file(file: File) {
    // Native platforms must close the sibling before remove/rename (notably
    // on Windows). The wasm32 std facade has no file descriptor and its File
    // stub does not implement Drop, but consuming the value keeps this shared
    // source warning-clean for browser bindings.
    #[cfg(not(target_arch = "wasm32"))]
    drop(file);
    #[cfg(target_arch = "wasm32")]
    let _ = file;
}
