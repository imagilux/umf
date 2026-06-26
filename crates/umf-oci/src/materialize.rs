//! Materialize an OCI image's layers into a rootfs directory.
//!
//! Applies layer tarballs in manifest order to a target directory, honouring
//! OCI whiteouts (`.wh.<name>` deletions and `.wh..wh..opq` opaque-directory
//! markers) so the result is the merged filesystem view. Where
//! [`crate::staging::BuildStaging`] accumulates an in-progress build tree, this
//! is the read side: it turns a *finished* image into a concrete rootfs that
//! the projector (`umf compile`) writes onto a disk's root partition.
//!
//! Lean by design — depends only on the layout's blob store, never on the
//! container engine. The boreal deploy path and `umf compile` both rely on
//! this staying engine-free.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use thiserror::Error;
use tracing::debug;

use crate::format::{self, Format};
use crate::registry::ImageLayout;
use crate::registry::error::RegistryError;

/// OCI whiteout prefix: `.wh.<name>` deletes `<name>` from the lower layers.
const WH_PREFIX: &str = ".wh.";
/// OCI opaque marker: `.wh..wh..opq` clears the containing directory's
/// lower-layer contents before this layer's entries apply.
const WH_OPAQUE: &str = ".wh..wh..opq";

/// Errors produced while materializing layers into a rootfs.
#[derive(Debug, Error)]
pub enum MaterializeError {
    /// Resolving a layer blob in the layout failed.
    #[error("resolving layer blob: {0}")]
    Layout(#[from] RegistryError),

    /// Applying a specific layer failed.
    #[error("applying layer {digest}: {source}")]
    Layer {
        /// Digest of the layer that failed.
        digest: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// I/O error creating the target or opening a blob.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

/// Apply each layer of `layer_digests` (in order) onto `target`, producing the
/// merged rootfs. Each digest names a (possibly gzipped) tar blob in `layout`.
/// Whiteouts are honoured, so deletions in upper layers remove files
/// contributed by lower ones.
pub fn materialize_layers<S: AsRef<str>>(
    layout: &ImageLayout,
    layer_digests: &[S],
    target: &Path,
) -> Result<(), MaterializeError> {
    fs::create_dir_all(target)?;
    for d in layer_digests {
        let digest = d.as_ref();
        let path = layout.blob_path(digest)?;
        let file = fs::File::open(&path)?;
        apply_layer(file, target).map_err(|source| MaterializeError::Layer {
            digest: digest.to_string(),
            source,
        })?;
        debug!(layer = %digest, "applied layer to rootfs");
    }
    Ok(())
}

/// Apply a single layer tar stream onto `target`, processing OCI whiteouts.
///
/// The stream may be gzip- or zstd-compressed (the two OCI layer codecs,
/// `+gzip` / `+zstd`) or an uncompressed tar; the codec is fingerprinted from
/// the leading magic via [`crate::format::detect`] rather than the descriptor's
/// media type, so a hand-rolled layer is handled the same way. Public for
/// callers that already hold the layer bytes (and for targeted tests);
/// [`materialize_layers`] is the usual entry point.
pub fn apply_layer<R: Read>(source: R, target: &Path) -> io::Result<()> {
    // Peek the leading bytes (enough for the longest compression magic we route
    // on — zstd's 4) and chain them back ahead of the rest, so the codec sniff
    // never consumes data the decoder needs. Same trick as `staging`, kept local
    // so this module stays self-contained.
    let mut reader = io::BufReader::new(source);
    let mut peek = [0u8; 4];
    let n = read_full(&mut reader, &mut peek)?;
    let combined = (&peek[..n]).chain(reader);
    match format::detect(&peek[..n]) {
        Format::Gzip => apply_tar(GzDecoder::new(combined), target),
        Format::Zstd => apply_tar(zstd::stream::read::Decoder::new(combined)?, target),
        // A 4-byte prefix can't reach the `ustar` magic at offset 257, so an
        // uncompressed tar fingerprints as `Unknown` here — that's fine, it
        // falls through to the plain-tar branch.
        _ => apply_tar(combined, target),
    }
}

fn apply_tar<R: Read>(source: R, target: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(source);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);

    // Whiteout application must not depend on intra-layer tar ordering. OCI
    // opaque semantics hide only *lower-layer* content, so a `d/.wh..wh..opq`
    // marker must clear what earlier layers put in `d` while leaving this
    // layer's own `d/*` entries intact — regardless of whether the marker
    // precedes or follows those siblings in the stream (Docker/buildkit emit it
    // first, but a hand-rolled layer may not). We therefore make a single pass
    // that unpacks ordinary entries and *records* the whiteouts, then apply the
    // deletions afterwards, excluding everything this layer wrote. This needs no
    // buffering of entry data and keeps the `safe_descend` containment intact.
    let mut opaque_dirs: Vec<PathBuf> = Vec::new();
    // Absolute paths this layer wrote, so a deferred opaque clear spares
    // same-layer siblings (only lower-layer contents are removed).
    let mut written: HashSet<PathBuf> = HashSet::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        match classify(&path) {
            Whiteout::Opaque(dir) => {
                // `false`: the dir itself must be real — `clear_dir` reads
                // through a final symlink, so a symlinked opaque target is
                // skipped rather than followed out of `target`. Recorded and
                // applied after the unpack pass: the clear must spare this
                // layer's own `dir/*` entries no matter the stream order.
                if let Some(p) = safe_descend(target, &dir, false)? {
                    opaque_dirs.push(p);
                }
            }
            Whiteout::Remove(victim) => {
                // `true`: the victim itself may be a symlink (a whiteout that
                // replaces a symlink entry) — `remove_path` unlinks it directly
                // without following it. Applied inline: a `.wh.<name>` names one
                // lower-layer path, so its effect is already order-independent.
                if let Some(p) = safe_descend(target, &victim, true)? {
                    remove_path(&p)?;
                }
            }
            Whiteout::None => {
                // `unpack_in` returns false when it refuses an unsafe path
                // (absolute / traversal); we let it skip those silently. Record
                // the destination so a later opaque clear keeps it.
                if entry.unpack_in(target)? {
                    written.insert(target.join(&path));
                }
            }
        }
    }

    // Apply deferred opaque clears, sparing this layer's own writes.
    for dir in &opaque_dirs {
        clear_dir(dir, &written)?;
    }
    Ok(())
}

/// Whiteout classification of a layer entry by its path.
enum Whiteout {
    /// `.wh..wh..opq` in a directory — clear that directory's existing contents.
    Opaque(PathBuf),
    /// `.wh.<name>` — remove `<dir>/<name>`.
    Remove(PathBuf),
    /// An ordinary entry.
    None,
}

fn classify(path: &Path) -> Whiteout {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Whiteout::None;
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if name == WH_OPAQUE {
        Whiteout::Opaque(parent.to_path_buf())
    } else if let Some(original) = name.strip_prefix(WH_PREFIX) {
        Whiteout::Remove(parent.join(original))
    } else {
        Whiteout::None
    }
}

/// Resolve a whiteout target *inside* `base`, refusing to traverse symlinks.
///
/// Whiteout deletes (`clear_dir` / `remove_path`) bypass tar's `unpack_in`
/// traversal guard, so this is the containment boundary. It rejects
/// `..`/absolute components, then walks the path component by component: if an
/// **intermediate** component is a symlink (a layer can plant `evil -> /host`,
/// and the OS would otherwise follow it during the delete), or a component
/// doesn't exist, it returns `None` — the delete is skipped rather than allowed
/// to escape `base`. `allow_final_symlink` lets the *last* component be a
/// symlink: a `.wh.<name>` removing a symlink entry unlinks it directly without
/// traversal, whereas an opaque-dir clear reads *through* a final symlink and so
/// passes `false`.
fn safe_descend(base: &Path, rel: &Path, allow_final_symlink: bool) -> io::Result<Option<PathBuf>> {
    let mut normals = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => normals.push(c),
            Component::CurDir => {}
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe path component {other:?} in layer entry {rel:?}"),
                ));
            }
        }
    }
    let mut cur = base.to_path_buf();
    for (i, c) in normals.iter().enumerate() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let is_final = i + 1 == normals.len();
                return Ok((is_final && allow_final_symlink).then_some(cur));
            }
            Ok(_) => {}
            // Nothing below an absent component to delete.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(cur))
}

/// Remove every entry inside `dir` (opaque-directory semantics) **except** the
/// paths in `keep` — those the current layer wrote, which opaque markers must
/// not touch (only lower-layer contents are hidden). No-op when the directory
/// doesn't exist yet (a lower layer hasn't created it).
fn clear_dir(dir: &Path, keep: &HashSet<PathBuf>) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let child = entry?.path();
        if keep.contains(&child) {
            continue;
        }
        remove_path(&child)?;
    }
    Ok(())
}

/// Remove a file, directory (recursively), or symlink; ignore if already gone.
fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read until `buf` is full or EOF; returns bytes filled. Shared with
/// [`crate::staging`], which uses it for the same gzip-magic peek.
pub(crate) fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests;
