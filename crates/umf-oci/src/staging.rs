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

use flate2::read::GzDecoder;
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

    /// The archive is compressed with a codec the staging unpacker can't
    /// decompress (only gzip and uncompressed tar are supported).
    #[error("unsupported archive compression `{0}` (staging unpacks gzip or plain tar only)")]
    UnsupportedCompression(&'static str),

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

    /// Unpack a (possibly gzipped) tar archive sitting at `tarball_path`
    /// into the staging tree.
    ///
    /// Detects gzip by checking for the magic bytes and transparently
    /// decompresses on the fly. Used for ROOTFS minirootfs tarballs from
    /// upstream distros + registry-cached rootfs artifacts.
    pub fn unpack_tarball(&mut self, tarball_path: &Path) -> Result<(), StagingError> {
        let file = File::open(tarball_path)?;
        self.unpack_tar_stream(file)
    }

    /// Unpack a (possibly gzipped) tar archive from an in-memory byte slice.
    /// Useful for tests that don't want to round-trip through a file.
    pub fn unpack_tar_bytes(&mut self, bytes: &[u8]) -> Result<(), StagingError> {
        self.unpack_tar_stream(bytes)
    }

    fn unpack_tar_stream<R: Read>(&self, source: R) -> Result<(), StagingError> {
        // Peek the leading bytes (enough for the longest compression magic, xz
        // at 6) and fingerprint via `format::detect`, so we route gzip to the
        // decoder, reject a compression we can't decompress with a clear error,
        // and otherwise treat the stream as a plain tar.
        let mut reader = std::io::BufReader::new(source);
        let mut peek = [0u8; 6];
        let peeked = crate::materialize::read_full(&mut reader, &mut peek)?;
        let format = crate::format::detect(&peek[..peeked]);

        // We can't easily put the peeked bytes back into the BufReader — work
        // around that by chaining a cursor over the peek with the rest of the
        // reader.
        let combined = std::io::Read::chain(&peek[..peeked], reader);

        // Cap the decompressed byte count so a gzip bomb can't fill the disk
        // (mirrors `materialize::apply_layer`).
        let cap = crate::materialize::max_uncompressed_layer_bytes();
        match format {
            crate::format::Format::Gzip => self.unpack_tar_into_staging(
                crate::materialize::CappedReader::new(GzDecoder::new(combined), cap),
            ),
            // A 6-byte prefix can't see the `ustar` magic at offset 257, so a
            // plain tar reads as `Unknown` here — that's fine, it falls through
            // to the uncompressed-tar branch below.
            f if f.is_compressed() => Err(StagingError::UnsupportedCompression(f.as_str())),
            _ => self.unpack_tar_into_staging(crate::materialize::CappedReader::new(combined, cap)),
        }
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
