//! erofs encoding for cached lower layers.
//!
//! Given an OCI layer blob (gzip+tar) already resident in an
//! [`ImageLayout`], produce a content-addressed `.erofs` image in the
//! layout's `cache/erofs/` sidecar. The erofs is keyed on the layer's
//! **diff_id** (the sha256 of the *uncompressed* tar), so a base layer
//! shared across images encodes once and is reused everywhere.
//!
//! The engine mounts these erofs files read-only and stacks them as
//! overlayfs lowers instead of unpacking every layer into a directory
//! tree on every build — see `umf-engine`'s `erofs` + `bundle` modules.
//!
//! ## Encoding
//!
//! `mkfs.erofs --tar=f --gzip --aufs <out> <layer.tar.gz>`:
//! - `--tar=f` streams a tarball straight into an erofs image (no
//!   intermediate directory tree — this is what lets the *cold* encode
//!   beat a per-file unpack of a 50k-file base).
//! - `--gzip` consumes the gzip-compressed OCI layer directly.
//! - `--aufs` converts the tar's aufs-style whiteouts (`.wh.<name>`,
//!   `.wh..wh..opq`) — which is exactly the OCI whiteout convention —
//!   into overlayfs metadata (character-device whiteouts + opaque
//!   xattrs), so the per-layer erofs images stack correctly under
//!   overlayfs.
//!
//! erofs is an *optional acceleration*: when `mkfs.erofs` is missing the
//! caller falls back to the unpack-to-directory path. [`encoder_available`]
//! reports whether the host can encode.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use tempfile::Builder;
use tracing::debug;

use crate::registry::ImageLayout;
use crate::registry::error::RegistryError;

/// Whether this host can encode layers to erofs: `mkfs.erofs` is on
/// `PATH` and advertises `--tar` support (erofs-utils ≥ 1.6). Probed
/// once and memoized.
#[must_use]
pub fn encoder_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // `mkfs.erofs --help` prints its usage (including the supported
        // option list) and exits non-zero; capture both streams and
        // require the `--tar` token. A missing binary → not available.
        let Ok(output) = Command::new("mkfs.erofs").arg("--help").output() else {
            return false;
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        text.contains("--tar")
    })
}

/// Ensure an erofs encoding of the layer with the given OCI `layer_digest`
/// (the gzip-tar blob digest) and `diff_id` (uncompressed-tar digest)
/// exists in `layout`'s erofs cache, returning its path.
///
/// Idempotent and content-addressed: if the cached erofs already exists
/// it is returned untouched (the cross-image dedup win). Otherwise the
/// layer's gzip-tar blob (already a file in the layout's blob store) is
/// encoded directly; the output is written to a temp file in the cache
/// directory and atomically renamed into place, so a crashed or
/// concurrent encode never leaves a partial erofs behind.
///
/// # Errors
/// [`RegistryError::Erofs`] if `mkfs.erofs` is unavailable or fails;
/// [`RegistryError::Io`] / [`RegistryError::MalformedDigest`] for layout
/// access. On any error the caller should fall back to the unpack path.
pub fn ensure_layer_erofs(
    layout: &ImageLayout,
    layer_digest: &str,
    diff_id: &str,
) -> Result<PathBuf, RegistryError> {
    let out = layout.erofs_cache_path(diff_id)?;
    // Content-addressed cache hit: a non-empty file is a complete encode
    // (writes are atomic — see below), so reuse it verbatim.
    if out.metadata().is_ok_and(|m| m.len() > 0) {
        debug!(diff_id, path = %out.display(), "erofs cache hit");
        return Ok(out);
    }

    let parent = out.parent().ok_or_else(|| {
        RegistryError::Erofs(format!("erofs cache path has no parent: {}", out.display()))
    })?;
    std::fs::create_dir_all(parent)?;

    // Feed the layer's gzip-tar blob to mkfs.erofs straight from the
    // layout's blob store — it's already a content-addressed file on
    // disk (verified when pulled), so there's no need to copy it.
    let blob_file = layout.blob_path(layer_digest)?;
    if !blob_file.is_file() {
        return Err(RegistryError::Erofs(format!(
            "layer blob {layer_digest} not present in layout"
        )));
    }

    // Encode into a temp erofs in the same dir, then atomically rename.
    let erofs_tmp = Builder::new()
        .prefix(".erofs-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    let erofs_tmp_path = erofs_tmp.path().to_path_buf();

    let output = Command::new("mkfs.erofs")
        .arg("--tar=f")
        .arg("--gzip")
        .arg("--aufs")
        .arg(&erofs_tmp_path)
        .arg(&blob_file)
        .output()
        .map_err(|e| RegistryError::Erofs(format!("spawning mkfs.erofs: {e}")))?;
    if !output.status.success() {
        return Err(RegistryError::Erofs(format!(
            "mkfs.erofs failed (status={}, diff_id={diff_id}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }

    // `mkfs.erofs` wrote into the temp path; persist (rename) it into the
    // content-addressed slot. A concurrent encoder racing to the same
    // diff_id produces identical bytes, so a last-writer-wins rename is safe.
    erofs_tmp
        .persist(&out)
        .map_err(|e| RegistryError::Io(e.error))?;
    debug!(diff_id, path = %out.display(), "erofs layer encoded");
    Ok(out)
}

#[cfg(test)]
mod tests;
