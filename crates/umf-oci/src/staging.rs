//! Build staging directory: the in-progress L1+ union of a VM build.
//!
//! Round 3. A [`BuildStaging`] owns a tempdir into which every layer above
//! L0 — ROOTFS first, then KERNEL files, then INITRD, then RUN-step diffs,
//! then runtime-config writes — accumulates files. When the build finishes,
//! the disk-emission step copies the resulting tree into the VM disk
//! image's ROOTFS partition.
//!
//! The tempdir is dropped automatically when the [`BuildStaging`] goes out
//! of scope; partial builds leave nothing behind.
//!
//! This module only exposes the staging container plus low-level helpers
//! for adding content. The directive-specific logic (resolve ROOTFS,
//! install KERNEL modules into `/lib/modules`, …) lives next to the
//! resolver that produces the source bytes, over in `umf-builder`.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;
use tracing::debug;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by [`BuildStaging`] operations.
#[derive(Debug, Error)]
pub enum StagingError {
    /// Tar archive unpacking failed (corrupt tar, bad permissions, …).
    #[error("unpacking tar: {0}")]
    Unpack(#[source] io::Error),

    /// The stream is a recognised format that is not a tar and cannot be
    /// unpacked as one (a zip, which is a seekable container — route it
    /// through [`BuildStaging::unpack_zip`] — or a squashfs image), or its
    /// decoder could not be started.
    #[error(transparent)]
    Decode(#[from] crate::compression::DecodeError),

    /// Zip archive unpacking failed (corrupt central directory, bad entry,
    /// unsupported entry codec, …).
    #[error("unpacking zip: {0}")]
    Zip(#[from] zip::result::ZipError),

    /// A zip entry would land outside the extraction root — an absolute
    /// name, a `..` component, or a symlink whose placement resolves out of
    /// the tree. The archive is malformed or hostile; refused, not skipped.
    #[error("zip entry `{name}` escapes the extraction root")]
    ZipEntryEscape {
        /// The offending entry name as stored in the archive.
        name: String,
    },

    /// I/O error opening or reading a tarball source path.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

// ── BuildStaging ────────────────────────────────────────────────────────────

/// Owned tempdir representing the in-progress L1+ tree of a VM build.
///
/// Construct with [`Self::new`], which eagerly creates the backing tempdir
/// (`mkdtemp`); it is removed when the [`BuildStaging`] is dropped.
#[derive(Debug)]
pub struct BuildStaging {
    dir: TempDir,
}

impl BuildStaging {
    /// Create an empty staging directory in the system temp dir.
    pub fn new() -> Result<Self, StagingError> {
        let dir = TempDir::with_prefix("umf-staging-")?;
        debug!(path = %dir.path().display(), "staging directory created");
        Ok(Self { dir })
    }

    /// Filesystem path the staging tree lives at. Stable across the
    /// staging's lifetime.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Persist the staging directory by detaching it from the
    /// auto-cleanup, returning its path. The caller becomes responsible for
    /// removing it — primarily useful for inspection during debugging or
    /// for handing the path to a follow-up tool that consumes the tree.
    pub fn into_path(self) -> PathBuf {
        self.dir.keep()
    }

    /// Unpack a (possibly compressed) tar archive sitting at `tarball_path`
    /// into the staging tree.
    ///
    /// The compression codec — gzip, zstd, xz, or bzip2 — is fingerprinted
    /// from the leading magic bytes and decoded transparently on the fly.
    /// Used for ROOTFS minirootfs tarballs from upstream distros,
    /// registry-cached rootfs artifacts, and fetched `ADD <url>` payloads.
    pub fn unpack_tarball(&mut self, tarball_path: &Path) -> Result<(), StagingError> {
        let file = File::open(tarball_path)?;
        self.unpack_tar_stream(file)
    }

    /// Unpack a (possibly compressed) tar archive from an in-memory byte
    /// slice. Useful for tests that don't want to round-trip through a file.
    pub fn unpack_tar_bytes(&mut self, bytes: &[u8]) -> Result<(), StagingError> {
        self.unpack_tar_stream(bytes)
    }

    fn unpack_tar_stream<R: Read>(&self, source: R) -> Result<(), StagingError> {
        // Codec selection lives in `crate::compression`, shared with the layer
        // reader, so a fetched payload and a layer blob accept the same set —
        // plain tar, or a gzip/zstd/xz/bzip2-compressed tar.
        let (_format, decoded) = crate::compression::decode_tar_stream(source)?;
        self.unpack_tar_into_staging(decoded)
    }

    /// Unpack a zip archive sitting at `zip_path` into the staging tree.
    ///
    /// A zip's index (the central directory) lives at the *end* of the file,
    /// so unlike the tar path this reads from a seekable path, not a stream.
    /// The tar unpack's guarantees hold here too: entry names are contained
    /// (an absolute or `..`-escaping name fails the unpack rather than being
    /// skipped), the cumulative decompressed byte count is capped so a zip
    /// bomb can't fill the disk, and unix permission bits are preserved when
    /// the archive carries them (same supply-chain stance as the tar path's
    /// `set_preserve_permissions`). Symlink entries are created only after
    /// every regular file has landed, so no file write can be redirected
    /// through an archive-controlled link; link *targets* are stored as-is
    /// (they resolve inside the image at runtime, exactly as tar symlinks
    /// do), while the link's own placement is re-checked against the root.
    pub fn unpack_zip(&mut self, zip_path: &Path) -> Result<(), StagingError> {
        use std::os::unix::fs::PermissionsExt as _;

        const S_IFMT: u32 = 0o170000;
        const S_IFLNK: u32 = 0o120000;

        let file = File::open(zip_path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        // One cumulative ceiling across the whole archive — the tar paths cap
        // their single decompressed stream the same way.
        let mut remaining = crate::materialize::max_uncompressed_layer_bytes();
        // Deferred symlinks: (name as stored, destination, link target).
        let mut symlinks: Vec<(String, PathBuf, PathBuf)> = Vec::new();

        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            // Containment: `enclosed_name` refuses absolute names, `..`
            // components, and NUL bytes.
            let Some(rel) = entry.enclosed_name() else {
                return Err(StagingError::ZipEntryEscape {
                    name: entry.name().to_string(),
                });
            };
            let dest = self.dir.path().join(&rel);

            if entry.is_dir() {
                std::fs::create_dir_all(&dest)?;
                continue;
            }

            let mode = entry.unix_mode();
            if mode.is_some_and(|m| m & S_IFMT == S_IFLNK) {
                // The entry body is the link target; cap the read at a
                // PATH_MAX-ish bound so a bogus header can't balloon it.
                let mut target = String::new();
                (&mut entry).take(4096).read_to_string(&mut target)?;
                symlinks.push((entry.name().to_string(), dest, PathBuf::from(target)));
                continue;
            }

            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&dest)?;
            let copied = io::copy(
                &mut crate::materialize::CappedReader::new(&mut entry, remaining),
                &mut out,
            )
            .map_err(StagingError::Unpack)?;
            remaining -= copied;
            if let Some(m) = mode {
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(m & 0o7777))?;
            }
        }

        // Symlinks land last, so none of the file writes above could have
        // traversed one. Placement is still contained: with links now able to
        // appear under other links, canonicalize each parent and require it
        // to stay inside the staging root.
        let root = self.dir.path().canonicalize()?;
        for (name, dest, target) in symlinks {
            let parent = dest.parent().unwrap_or(&root);
            std::fs::create_dir_all(parent)?;
            if !parent.canonicalize()?.starts_with(&root) {
                return Err(StagingError::ZipEntryEscape { name });
            }
            // Last-writer-wins on a colliding earlier entry, as tar has.
            match std::fs::remove_file(&dest) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            std::os::unix::fs::symlink(&target, &dest)?;
        }
        Ok(())
    }

    fn unpack_tar_into_staging<R: Read>(&self, source: R) -> Result<(), StagingError> {
        let mut archive = tar::Archive::new(source);
        // `set_preserve_permissions(true)` keeps mode bits — **including
        // setuid/setgid** — because a bootable rootfs legitimately needs them
        // (`su`, `sudo`, `ping`, …). A consequence: a malicious ROOTFS artifact
        // can introduce SUID-root binaries into the image. That is supply-chain
        // trust in the ROOTFS reference, mitigated by the OCI digest model
        // (pull verifies content against the digest), not a traversal flaw —
        // `unpack` itself is traversal-safe (`tar` canonicalizes each entry's
        // parent, refusing `..`/absolute/symlink escapes). xattrs are dropped.
        archive.set_preserve_permissions(true);
        archive.set_unpack_xattrs(false);
        archive
            .unpack(self.dir.path())
            .map_err(StagingError::Unpack)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
