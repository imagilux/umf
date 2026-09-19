//! ROOTFS partition writer — pack a rootfs directory tree as a filesystem
//! image and write it into a partition slice of the disk.
//!
//! Three filesystems are supported, selected at projection time by
//! `umf compile --fs` (see [`umf_core::boot::RootfsFs`]):
//!
//! - **squashfs** (default) — read-only, compressed, written *in-process*
//!   via [`backhand`]. No host tooling, so the default path keeps `umf
//!   compile` pure-Rust and usable on an air-gapped node with nothing
//!   installed.
//! - **ext4** — read-write, uncompressed, written by the host's `mkfs.ext4`.
//!   The choice when the root is meant to be remounted writable in place.
//! - **erofs** — read-only, compressed, written by the host's `mkfs.erofs`.
//!
//! ## Why the other two shell out
//!
//! Neither ext4 nor erofs has a mature pure-Rust *writer*; `mkfs.ext4`
//! (e2fsprogs) and `mkfs.erofs` (erofs-utils) are standard on every
//! distribution, and both populate an image from a directory without
//! privileges — `mkfs.ext4 -d` and `mkfs.erofs <out> <src>` write inodes
//! into a regular file directly rather than going through the kernel, so
//! they record uid/gid/mode (and even device nodes) faithfully as an
//! unprivileged user.
//!
//! This is a deliberate and *narrow* exception to UMF's in-process posture.
//! Unlike the erofs **layer cache** in `umf-oci`, which falls back to a
//! pure-Rust unpack when `mkfs.erofs` is missing and is therefore pure
//! acceleration, there is no fallback here: a root partition the operator
//! asked to be ext4 must not be silently written as something else, so a
//! missing tool is a hard error naming the package to install.
//!
//! ## On-disk form
//!
//! The partition holds the raw filesystem bytes (no outer header), starting
//! at the partition's first sector. The kernel mounts it per the
//! `root=PARTLABEL=ROOTFS rootfstype=<fs>` cmdline `umf compile` writes —
//! the projector is the single writer of both the bytes and the cmdline, so
//! the two cannot disagree.

use std::fs::Metadata;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use backhand::{FilesystemWriter, NodeHeader};
use thiserror::Error;
use tracing::{debug, info};
use umf_core::boot::RootfsFs;
use walkdir::WalkDir;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by [`write_rootfs_from_dir`].
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

    /// The `mkfs` binary for the requested filesystem is not on `PATH`.
    /// There is deliberately no fallback — see the module docs.
    #[error(
        "`{tool}` is required to write a {fs} root partition but was not found on PATH; \
         install it ({package}), or project with `--fs squashfs`, which needs no host tooling"
    )]
    MkfsUnavailable {
        /// The binary that was looked for.
        tool: &'static str,
        /// The filesystem that needed it.
        fs: RootfsFs,
        /// Distribution package hint.
        package: &'static str,
    },

    /// The `mkfs` binary ran and failed. `stderr` is carried verbatim: it is
    /// the only place the real reason appears.
    #[error("`{tool}` failed ({status}): {stderr}")]
    MkfsFailed {
        /// The binary that failed.
        tool: &'static str,
        /// Its exit status, rendered.
        status: String,
        /// Whatever it wrote to stderr, trimmed.
        stderr: String,
    },

    /// The produced image does not fit the partition it must be written into.
    #[error(
        "{fs} image is {image} bytes but the ROOTFS partition holds only {capacity}; \
         project a larger disk with `--disk-size`"
    )]
    ImageTooLarge {
        /// The filesystem that was written.
        fs: RootfsFs,
        /// Size of the produced image.
        image: u64,
        /// Size of the target partition.
        capacity: u64,
    },
}

impl From<backhand::BackhandError> for FilesystemError {
    fn from(value: backhand::BackhandError) -> Self {
        Self::Squashfs(value.to_string())
    }
}

// ── Result ──────────────────────────────────────────────────────────────────

/// Per-node counts from a traversal this crate performed itself.
#[derive(Debug, Clone, Default)]
pub struct NodeCounts {
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

/// Summary of a ROOTFS write.
#[derive(Debug, Clone)]
pub struct RootfsWriteReport {
    /// Which filesystem was written.
    pub fs: RootfsFs,
    /// Bytes of filesystem image placed in the partition.
    pub image_bytes: u64,
    /// Per-node counts — `Some` only for squashfs, whose tree this crate
    /// walks itself. `mkfs.ext4` / `mkfs.erofs` do their own traversal and
    /// report nothing per-node; re-walking the tree to produce numbers here
    /// would describe what *we* would have written, not what the tool did,
    /// so the field is honestly absent instead.
    pub nodes: Option<NodeCounts>,
}

/// Back-compat alias for the squashfs-only report shape.
pub type SquashfsWriteReport = NodeCounts;

// ── Public entry ────────────────────────────────────────────────────────────

/// Write the rootfs directory `root` into `output` as a `fs` filesystem image,
/// where `output` is a partition of `capacity` bytes.
///
/// This is the single entry point the projector uses; which writer runs is
/// decided here and nowhere else, so the caller never has to know that two of
/// the three shell out.
///
/// `output` is whatever `Write + Seek` device the caller hands us — in the
/// projection flow that's a [`crate::partition::PartitionView`] slice of the
/// disk image, but tests pass `Cursor<Vec<u8>>` so the writer stays decoupled
/// from the disk-layout machinery.
///
/// # Errors
/// [`FilesystemError::MkfsUnavailable`] when the host lacks the tool for a
/// non-default filesystem, [`FilesystemError::MkfsFailed`] when it runs and
/// fails, [`FilesystemError::ImageTooLarge`] when the result does not fit the
/// partition, and [`FilesystemError::Io`] / [`FilesystemError::Squashfs`] /
/// [`FilesystemError::Walk`] for the in-process path.
pub fn write_rootfs_from_dir<W: Write + Seek>(
    fs: RootfsFs,
    root: &Path,
    output: &mut W,
    capacity: u64,
) -> Result<RootfsWriteReport, FilesystemError> {
    match fs {
        RootfsFs::Squashfs => {
            // The image size is the furthest offset written, *not* the final
            // stream position: backhand seeks back to offset 0 to lay down the
            // superblock once the rest is known, so it leaves the stream ~96
            // bytes in regardless of how large the image actually is.
            let mut tracked = HighWater::new(output);
            let nodes = write_squashfs_from_dir(root, &mut tracked)?;
            let image_bytes = tracked.high_water();
            if image_bytes > capacity {
                return Err(FilesystemError::ImageTooLarge {
                    fs,
                    image: image_bytes,
                    capacity,
                });
            }
            Ok(RootfsWriteReport {
                fs,
                image_bytes,
                nodes: Some(nodes),
            })
        }
        RootfsFs::Ext4 | RootfsFs::Erofs => write_via_host_mkfs(fs, root, output, capacity),
    }
}

/// Package hint per filesystem, for the "tool missing" error. Kept beside the
/// tool name rather than in `umf-core` because it is a property of *how this
/// crate invokes the tool*, not of the format.
const fn package_hint(fs: RootfsFs) -> &'static str {
    match fs {
        RootfsFs::Ext4 => "e2fsprogs",
        RootfsFs::Erofs => "erofs-utils",
        RootfsFs::Squashfs => "",
    }
}

/// Build the image with the host `mkfs` for `fs`, then copy it into `output`.
///
/// The tool writes to a temp file rather than straight into the partition
/// because neither `mkfs.ext4` nor `mkfs.erofs` can target a byte range of a
/// larger file — there is no offset option, and the alternative (a loop device
/// with `--offset`) needs privileges the rest of this path does not.
fn write_via_host_mkfs<W: Write + Seek>(
    fs: RootfsFs,
    root: &Path,
    output: &mut W,
    capacity: u64,
) -> Result<RootfsWriteReport, FilesystemError> {
    let Some(tool) = fs.host_mkfs() else {
        // Unreachable via `write_rootfs_from_dir`, which routes squashfs to
        // the in-process writer, but an explicit arm beats an `unwrap`.
        return Err(FilesystemError::MkfsUnavailable {
            tool: "mkfs",
            fs,
            package: package_hint(fs),
        });
    };
    if !tool_on_path(tool) {
        return Err(FilesystemError::MkfsUnavailable {
            tool,
            fs,
            package: package_hint(fs),
        });
    }

    info!(root = %root.display(), %fs, tool, "rootfs: packing directory via host mkfs");

    let scratch = tempfile::tempdir()?;
    let image = scratch.path().join("rootfs.img");

    let mut command = std::process::Command::new(tool);
    match fs {
        RootfsFs::Ext4 => {
            // ext4 is a fixed-size filesystem, so it is sized to the partition
            // it will live in. Pre-creating the file sparse means mke2fs sizes
            // from it and no explicit block count has to be computed here.
            let file = std::fs::File::create(&image)?;
            file.set_len(capacity)?;
            drop(file);
            // `-q` silences the summary; `-d` populates from the directory,
            // writing inodes straight into the image (so uid/gid, modes and
            // device nodes survive without privileges); `-F` suppresses the
            // "not a block device" confirmation on a regular file.
            command.arg("-q").arg("-F").arg("-d").arg(root).arg(&image);
        }
        RootfsFs::Erofs => {
            // erofs sizes itself from the content: output first, source second.
            command.arg(&image).arg(root);
        }
        RootfsFs::Squashfs => unreachable!("squashfs is written in-process"),
    }

    let out = command.output().map_err(|e| FilesystemError::MkfsFailed {
        tool,
        status: "spawn failed".to_string(),
        stderr: e.to_string(),
    })?;
    if !out.status.success() {
        return Err(FilesystemError::MkfsFailed {
            tool,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }

    let image_bytes = std::fs::metadata(&image)?.len();
    if image_bytes > capacity {
        return Err(FilesystemError::ImageTooLarge {
            fs,
            image: image_bytes,
            capacity,
        });
    }

    let mut produced = std::fs::File::open(&image)?;
    let written = copy_skipping_holes(&mut produced, output)?;

    info!(
        %fs,
        tool,
        image_bytes,
        written_bytes = written,
        "rootfs: image written",
    );
    Ok(RootfsWriteReport {
        fs,
        image_bytes,
        nodes: None,
    })
}

/// A `Write + Seek` wrapper that records the furthest offset ever written.
///
/// Needed because a writer that seeks backwards — as `backhand` does, writing
/// the squashfs superblock last — leaves the stream position nowhere near the
/// end of what it produced. Reading the position back would report an image of
/// 96 bytes for a multi-megabyte filesystem, so any size check against it
/// would silently always pass and any size *logged* from it would be wrong.
///
/// Seeks alone never raise the mark: seeking past the end writes nothing.
struct HighWater<'a, W> {
    inner: &'a mut W,
    pos: u64,
    high: u64,
}

impl<'a, W: Write + Seek> HighWater<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        Self {
            inner,
            pos: 0,
            high: 0,
        }
    }

    fn high_water(&self) -> u64 {
        self.high
    }
}

impl<W: Write + Seek> Write for HighWater<'_, W> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        self.high = self.high.max(self.pos);
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.inner.flush()
    }
}

impl<W: Write + Seek> Seek for HighWater<'_, W> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, std::io::Error> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

/// Whether `tool` resolves on `PATH`.
///
/// Probed by walking `PATH` rather than by spawning the tool: several `mkfs`
/// variants exit non-zero for `--help`/`--version` (erofs-utils has no
/// `--version` at all), so "it ran and failed" is not distinguishable from
/// "it is missing" by exit status alone.
fn tool_on_path(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(tool);
        // Only an executable regular file counts; a directory named
        // `mkfs.ext4` on PATH is not the tool.
        std::fs::metadata(&candidate).is_ok_and(|m| m.is_file()) && rustix_is_executable(&candidate)
    })
}

fn rustix_is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Copy `src` into `dst`, seeking over runs of zero bytes instead of writing
/// them.
///
/// `mkfs.ext4` sizes its image to the whole partition, most of which is empty;
/// writing those zeros verbatim would materialise every block of the disk
/// image and cost UMF its "build output is always a sparse image" property.
/// The destination partition starts out as a hole in a freshly `set_len`'d
/// disk file, so skipping a zero run leaves it a hole.
///
/// Returns the number of bytes actually written (excluding skipped holes).
fn copy_skipping_holes<R: Read, W: Write + Seek>(
    src: &mut R,
    dst: &mut W,
) -> Result<u64, std::io::Error> {
    // 64 KiB: large enough that the zero check is cheap per byte, small enough
    // that a sparse region interleaved with data still yields holes.
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    let mut pending_skip = 0i64;

    loop {
        let n = read_full(src, &mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if chunk.iter().all(|b| *b == 0) {
            // Defer the seek: consecutive holes collapse into one.
            pending_skip += n as i64;
            continue;
        }
        if pending_skip != 0 {
            dst.seek(SeekFrom::Current(pending_skip))?;
            pending_skip = 0;
        }
        dst.write_all(chunk)?;
        written += n as u64;
    }

    // A trailing zero run needs no seek: the partition is already that long,
    // and seeking past the end of a `Cursor` would not extend it anyway.
    dst.flush()?;
    Ok(written)
}

/// Read until `buf` is full or EOF, returning how much was filled.
///
/// Correctness does not depend on this — the loop slices to what was actually
/// read, so a short read is a smaller chunk and nothing more. It is here so
/// holes *coalesce*: a reader dribbling 7 bytes at a time would otherwise turn
/// one megabyte-long hole into ~150k separate seeks.
fn read_full<R: Read>(src: &mut R, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

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
) -> Result<NodeCounts, FilesystemError> {
    info!(root = %root.display(), "rootfs: packing directory into squashfs");

    let mut writer = FilesystemWriter::default();
    let mut report = NodeCounts {
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
pub(crate) mod tests;
