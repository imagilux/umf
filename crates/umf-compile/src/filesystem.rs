//! ROOTFS partition writer — pack a rootfs directory tree as a SquashFS image
//! and write it into a partition slice of the disk.
//!
//! No Rust crate currently writes ext4, so we adopt SquashFS via [`backhand`].
//! SquashFS is read-only at boot, which suits the immutable-OS-image shape;
//! mutable-rootfs use cases (in-place upgrades) will need a follow-up that
//! swaps SquashFS for an ext4 emitter or a layered overlay scheme.
//!
//! The on-disk partition holds the raw SquashFS bytes (no outer filesystem
//! header), starting at the partition's first sector. The Linux kernel mounts
//! it directly with `root=/dev/vda2 rootfstype=squashfs` (the second partition
//! follows the ESP in the GPT layout).

use std::fs::Metadata;
use std::io::{Seek, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use backhand::{FilesystemWriter, NodeHeader};
use thiserror::Error;
use tracing::{debug, info};
use walkdir::WalkDir;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by [`write_squashfs_from_dir`].
#[derive(Debug, Error)]
pub enum FilesystemError {
    /// Underlying I/O error walking or reading the rootfs tree.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Underlying [`backhand`] error packing or writing the SquashFS image.
    #[error("squashfs: {0}")]
    Squashfs(String),

    /// `walkdir` traversal error.
    #[error("walking rootfs: {0}")]
    Walk(#[from] walkdir::Error),
}

impl From<backhand::BackhandError> for FilesystemError {
    fn from(value: backhand::BackhandError) -> Self {
        Self::Squashfs(value.to_string())
    }
}

// ── Result ──────────────────────────────────────────────────────────────────

/// Summary of a SquashFS write.
#[derive(Debug, Clone)]
pub struct SquashfsWriteReport {
    /// Number of regular files written.
    pub files: usize,
    /// Number of directories written (excluding the implicit root).
    pub directories: usize,
    /// Number of symlinks written.
    pub symlinks: usize,
    /// Number of entries skipped (character / block / fifo / socket nodes —
    /// unsupported; recreated at first boot by udev / a tmpfs on `/dev`).
    pub skipped_special_nodes: usize,
}

// ── Public entry ────────────────────────────────────────────────────────────

/// Pack the contents of the rootfs directory `root` into a SquashFS image and
/// write it to `output`.
///
/// Preserves Unix permissions (mode bits), ownership (uid/gid), and symlinks.
/// Special nodes (character/block/fifo/socket) are skipped — they're produced
/// at first boot by udev / a tmpfs on `/dev`, so they don't need to live in
/// the on-disk rootfs image.
///
/// `output` is whatever `Write + Seek` device the caller hands us — in the
/// projection flow that's a [`crate::partition::PartitionView`] slice of the
/// disk image, but tests pass `Cursor<Vec<u8>>` so the writer stays decoupled
/// from the disk-layout machinery.
pub fn write_squashfs_from_dir<W: Write + Seek>(
    root: &Path,
    output: &mut W,
) -> Result<SquashfsWriteReport, FilesystemError> {
    info!(root = %root.display(), "rootfs: packing directory into squashfs");

    let mut writer = FilesystemWriter::default();
    let mut report = SquashfsWriteReport {
        files: 0,
        directories: 0,
        symlinks: 0,
        skipped_special_nodes: 0,
    };

    // walkdir yields parent-before-children by default, so directories land
    // before their contents.
    for entry in WalkDir::new(root).min_depth(1).sort_by_file_name() {
        let entry = entry?;
        // `entry.path()` is always a child of `root` (walkdir guarantee), so
        // `strip_prefix` should never fail — but `.unwrap_or` keeps clippy's
        // expect_used quiet and the path defined under any corruption.
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let rel_path = absolute_rel(rel);
        let meta = entry.metadata()?;
        let header = node_header_from_metadata(&meta);

        if meta.file_type().is_dir() {
            writer.push_dir(&rel_path, header)?;
            report.directories += 1;
        } else if meta.file_type().is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            writer.push_symlink(target, &rel_path, header)?;
            report.symlinks += 1;
        } else if meta.file_type().is_file() {
            let file = std::fs::File::open(entry.path())?;
            writer.push_file(file, &rel_path, header)?;
            report.files += 1;
        } else {
            debug!(path = %rel_path.display(), "rootfs: skipping unsupported node");
            report.skipped_special_nodes += 1;
        }
    }

    writer.write(output)?;
    info!(
        files = report.files,
        dirs = report.directories,
        symlinks = report.symlinks,
        skipped = report.skipped_special_nodes,
        "rootfs: squashfs image written",
    );
    Ok(report)
}

fn absolute_rel(rel: &Path) -> PathBuf {
    // backhand wants paths rooted at `/`. WalkDir gives us `rel` without the
    // leading slash; prepend.
    let mut out = PathBuf::from("/");
    out.push(rel);
    out
}

fn node_header_from_metadata(meta: &Metadata) -> NodeHeader {
    NodeHeader {
        permissions: (meta.permissions().mode() & 0o7777) as u16,
        uid: meta.uid(),
        gid: meta.gid(),
        mtime: meta.mtime().try_into().unwrap_or(0),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
