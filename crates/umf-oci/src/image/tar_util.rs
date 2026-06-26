//! Internal: reproducible tar from a directory tree.
//!
//! Walks a directory in lexicographic order and packs it with
//! [`tar::HeaderMode::Deterministic`] so two calls with the same directory
//! contents produce byte-identical tar (and therefore identical diff_ids).
//! Consumed by [`super::LayerSource::from_directory`].

use std::fs::{File, read_dir};
use std::path::{Path, PathBuf};

use crate::registry::error::RegistryError;

/// Tar a directory tree into reproducible (deterministic-header) bytes.
pub(super) fn build_tar(root: &Path) -> Result<Vec<u8>, RegistryError> {
    let mut paths = Vec::new();
    collect_sorted(root, &mut paths)?;

    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        builder.mode(tar::HeaderMode::Deterministic);
        for path in &paths {
            let relative = path.strip_prefix(root).map_err(|e| {
                RegistryError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("strip_prefix: {e}"),
                ))
            })?;
            // Use symlink_metadata so we classify symlinks correctly
            // rather than chasing them and packing the target twice.
            let meta = std::fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(path)?;
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_uid(0);
                header.set_gid(0);
                header.set_size(0);
                header.set_mtime(0);
                header.set_cksum();
                builder.append_link(&mut header, relative, &target)?;
            } else if meta.is_dir() {
                builder.append_dir(relative, path)?;
            } else if meta.is_file() {
                let mut file = File::open(path)?;
                builder.append_file(relative, &mut file)?;
            }
            // Sockets, fifos and other special files are skipped — we
            // don't have a use case for them yet, and OCI tools tolerate
            // their absence in a layer tarball.
        }
        builder.finish()?;
    }
    Ok(tar_buf)
}

/// Collect every path under `dir` into `out` in lexicographic order, without
/// chasing symlinks (they are recorded as single entries).
fn collect_sorted(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), RegistryError> {
    let mut children: Vec<PathBuf> = read_dir(dir)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        out.push(child.clone());
        // Don't recurse into symlinks — we treat them as a single
        // entry. `is_dir` chases symlinks; `symlink_metadata` does not.
        let meta = std::fs::symlink_metadata(&child)?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            collect_sorted(&child, out)?;
        }
    }
    Ok(())
}
