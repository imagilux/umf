//! OCI Image Layout archive — save / load `.tar` files.
//!
//! Implements the [OCI Image Layout][spec] format: a tar archive
//! containing `oci-layout`, `index.json`, and `blobs/sha256/<hex>`
//! for every referenced blob. Round-trip compatible with
//! `skopeo copy oci-archive:` and `docker save --output` (with
//! `--platform`-style child manifest selection).
//!
//! ## Save
//!
//! [`save_to_writer`] takes a layout + a list of ref names and
//! produces a tar archive containing only those refs + the
//! transitive blob closure (manifest → config + layers, image-
//! indices → child manifests). Refs not present in the layout
//! error out; blob-closure walks reuse the same logic
//! [`ImageLayout::prune_blobs`] does for reachability.
//!
//! ## Load
//!
//! [`load_from_reader`] reads an OCI Image Layout tarball and
//! merges its blobs + index entries into an existing layout.
//! Collisions on ref name default to error unless `overwrite=true`;
//! collisions on blob digest are silent (content addressing —
//! equal digest implies equal content).
//!
//! [spec]: https://github.com/opencontainers/image-spec/blob/main/image-layout.md

use std::collections::HashSet;
use std::io::{Read, Write};

use serde::Deserialize;

use crate::registry::ImageLayout;
use crate::registry::error::RegistryError;

const OCI_LAYOUT_FILE: &str = "oci-layout";
const INDEX_JSON_FILE: &str = "index.json";
const BLOBS_PREFIX: &str = "blobs/sha256/";
const OCI_LAYOUT_PAYLOAD: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;

/// Hard per-entry byte ceiling while unpacking a `umf load` archive. A
/// crafted archive can declare a tiny header size and stream unbounded
/// bytes, so [`load_from_reader`] reads each entry through a cap and
/// aborts the instant it is exceeded — *before* the whole entry is
/// buffered — rather than collecting attacker-sized blobs into memory.
/// Mirrors the registry pull path's `MAX_BLOB_BYTES` (8 GiB): comfortably
/// larger than any real layer/config/manifest blob while bounding the
/// worst-case memory a single entry can consume.
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Errors specific to the archive format. Wraps [`RegistryError`] for
/// the underlying layout reads/writes plus `tar` / I/O errors.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// A requested ref isn't in the source layout.
    #[error("ref not in source layout: {0}")]
    RefNotFound(String),
    /// A ref already exists in the destination layout and
    /// `overwrite=false` was passed.
    #[error("ref already in destination layout: {0} (pass --overwrite to replace)")]
    RefCollision(String),
    /// The archive doesn't carry the mandatory `oci-layout` marker
    /// at its root.
    #[error("archive is missing the required `oci-layout` entry")]
    NotAnOciArchive,
    /// The archive's `index.json` couldn't be parsed.
    #[error("archive index.json: {0}")]
    BadIndex(serde_json::Error),
    /// Underlying layout read/write.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// I/O from tar packing or unpacking.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Tar archive malformed at the binary level.
    #[error("tar: {0}")]
    Tar(String),
}

/// Pack the selected refs (plus their transitive blob closure) from
/// `layout` into a tar archive written to `writer`.
///
/// The produced archive is compatible with the OCI Image Layout v1
/// spec — it contains `oci-layout`, `index.json` (filtered to only
/// the selected refs), and `blobs/sha256/<hex>` for every reachable
/// blob.
///
/// # Errors
/// [`ArchiveError::RefNotFound`] when a requested ref is missing
/// from the source layout; otherwise propagates I/O and registry
/// errors.
pub fn save_to_writer<W: Write>(
    layout: &ImageLayout,
    refs: &[String],
    writer: W,
) -> Result<(), ArchiveError> {
    let mut tar = tar::Builder::new(writer);

    // 1. `oci-layout` marker.
    write_string_entry(&mut tar, OCI_LAYOUT_FILE, OCI_LAYOUT_PAYLOAD)?;

    // 2. Filtered index.json — only the selected refs.
    let full_index = layout.read_index().map_err(ArchiveError::Registry)?;
    let mut filtered_manifests = Vec::new();
    let mut to_walk: Vec<String> = Vec::new();
    for ref_name in refs {
        let entry = full_index
            .manifests
            .iter()
            .find(|e| {
                e.annotations
                    .as_ref()
                    .and_then(|a| a.get("org.opencontainers.image.ref.name"))
                    .map(|s| s.as_str())
                    == Some(ref_name.as_str())
            })
            .ok_or_else(|| ArchiveError::RefNotFound(ref_name.clone()))?
            .clone();
        to_walk.push(entry.digest.clone());
        filtered_manifests.push(entry);
    }
    let filtered_index = oci_client::manifest::OciImageIndex {
        schema_version: full_index.schema_version,
        media_type: full_index.media_type.clone(),
        manifests: filtered_manifests,
        artifact_type: None,
        annotations: full_index.annotations.clone(),
    };
    let index_json = serde_json::to_vec_pretty(&filtered_index).map_err(ArchiveError::BadIndex)?;
    write_bytes_entry(&mut tar, INDEX_JSON_FILE, &index_json)?;

    // 3. Walk every reachable blob and pack it.
    let mut reachable: HashSet<String> = HashSet::new();
    for digest in &to_walk {
        layout.collect_reachable(digest, &mut reachable)?;
    }
    let mut sorted: Vec<_> = reachable.into_iter().collect();
    sorted.sort_unstable(); // deterministic archive ordering
    for digest in &sorted {
        let hex = digest
            .strip_prefix("sha256:")
            .ok_or_else(|| ArchiveError::Tar(format!("non-sha256 digest in layout: {digest}")))?;
        let path = format!("{BLOBS_PREFIX}{hex}");
        // Stream the blob straight from its on-disk path into the tar rather
        // than buffering the whole (possibly multi-GB) layer into a `Vec` and
        // re-hashing it via `read_blob`. The blob was content-verified when it
        // was written (`write_blob_with_digest`), so re-reading it through the
        // re-hashing `read_blob` was redundant work and a per-layer RSS spike.
        // The header is still built deterministically (mode/mtime/gnu format)
        // and the entries are still emitted in sorted-digest order, so the
        // archive remains byte-for-byte reproducible.
        let blob_path = layout.blob_path(digest)?;
        write_file_entry(&mut tar, &path, &blob_path)?;
    }

    tar.finish().map_err(|e| ArchiveError::Tar(e.to_string()))?;
    Ok(())
}

/// Unpack an OCI Image Layout tar archive read from `reader` and
/// merge its blobs + refs into `layout`.
///
/// Blob collisions are silent (content addressing). Ref collisions
/// default to [`ArchiveError::RefCollision`] unless `overwrite=true`,
/// in which case the new ref replaces the existing one.
///
/// # Errors
/// [`ArchiveError::NotAnOciArchive`] if the `oci-layout` marker is
/// missing; [`ArchiveError::RefCollision`] when overwrite is off and
/// a tag would clobber an existing one.
pub fn load_from_reader<R: Read>(
    layout: &ImageLayout,
    reader: R,
    overwrite: bool,
) -> Result<Vec<String>, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let mut saw_layout_marker = false;
    let mut index_bytes: Option<Vec<u8>> = None;
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let buf = read_entry_capped(&mut entry, &path)?;
        if path == OCI_LAYOUT_FILE {
            saw_layout_marker = true;
        } else if path == INDEX_JSON_FILE {
            index_bytes = Some(buf);
        } else if let Some(hex) = path.strip_prefix(BLOBS_PREFIX) {
            blobs.push((format!("sha256:{hex}"), buf));
        }
        // Silently skip directory entries + anything else (the OCI
        // spec only requires the above).
    }

    if !saw_layout_marker {
        return Err(ArchiveError::NotAnOciArchive);
    }
    let index_bytes = index_bytes.ok_or(ArchiveError::NotAnOciArchive)?;

    // Pre-flight: check for ref collisions before mutating the
    // destination so a collision in the middle of the load doesn't
    // leave the dest half-merged.
    let parsed_index: ArchiveIndex =
        serde_json::from_slice(&index_bytes).map_err(ArchiveError::BadIndex)?;
    if !overwrite {
        for manifest in &parsed_index.manifests {
            if let Some(ann) = &manifest.annotations
                && let Some(name) = ann.get("org.opencontainers.image.ref.name")
                && layout.lookup_ref(name)?.is_some()
            {
                return Err(ArchiveError::RefCollision(name.clone()));
            }
        }
    }

    // Write blobs (content-addressed; collisions are silent no-ops).
    for (digest, bytes) in &blobs {
        layout.write_blob_with_digest(bytes, digest)?;
    }

    // Upsert each ref.
    let mut loaded_refs = Vec::new();
    for entry in parsed_index.manifests {
        let ref_name = entry
            .annotations
            .as_ref()
            .and_then(|a| a.get("org.opencontainers.image.ref.name"))
            .cloned();
        // Convert the archive's per-manifest shape into oci-client's
        // ImageIndexEntry so we can reuse upsert_ref.
        let oci_entry = oci_client::manifest::ImageIndexEntry {
            media_type: entry.media_type,
            digest: entry.digest.clone(),
            size: entry.size,
            platform: None,
            annotations: entry.annotations,
        };
        if let Some(name) = ref_name {
            layout.upsert_ref(&name, oci_entry)?;
            loaded_refs.push(name);
        }
        // Annotation-less entries (rare) get dropped; the OCI spec
        // doesn't require every manifest entry to be tagged, but
        // without a ref name there's no useful way to surface it
        // from `umf images`.
    }
    Ok(loaded_refs)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Read a single tar entry into memory, refusing to buffer more than
/// [`MAX_ENTRY_BYTES`]. The read is bounded by `Read::take` so a crafted
/// entry can't grow the buffer unboundedly: we read at most the ceiling
/// plus one byte, and treat any overflow (that extra byte materialising)
/// as a hard rejection *before* the rest of the entry is consumed.
fn read_entry_capped<R: Read>(entry: &mut R, path: &str) -> Result<Vec<u8>, ArchiveError> {
    read_capped(entry, path, MAX_ENTRY_BYTES)
}

/// Cap parameterised so the overflow boundary is unit-testable with a tiny
/// ceiling (the production path passes [`MAX_ENTRY_BYTES`]).
fn read_capped<R: Read>(entry: &mut R, path: &str, cap: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut buf = Vec::new();
    // +1 so an entry exactly at the ceiling is accepted but one byte over is
    // detectable without buffering the whole oversized entry.
    let read = entry.take(cap + 1).read_to_end(&mut buf)?;
    if read as u64 > cap {
        return Err(ArchiveError::Tar(format!(
            "archive entry {path:?} exceeds the {cap}-byte ceiling"
        )));
    }
    Ok(buf)
}

fn write_bytes_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<(), ArchiveError> {
    let mut header = tar::Header::new_gnu();
    header.set_path(path).map_err(ArchiveError::Io)?;
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    tar.append(&header, bytes).map_err(ArchiveError::Io)?;
    Ok(())
}

/// Append a tar entry whose body is streamed from `src` on disk, instead of
/// reading the file into a `Vec` first.
///
/// The header is built identically to [`write_bytes_entry`] (mode `0o644`,
/// `mtime` 0, GNU format) so a blob packed this way is byte-for-byte the same
/// as if it had been buffered — only the body is fed from a [`std::fs::File`]
/// via [`tar::Builder::append`] (an `R: Read` sink) rather than a `&[u8]`. The
/// declared entry size is taken from the file's own metadata length, which the
/// `tar` writer requires to match the bytes actually streamed.
fn write_file_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    src: &std::path::Path,
) -> Result<(), ArchiveError> {
    let file = std::fs::File::open(src)?;
    let len = file.metadata()?.len();
    let mut header = tar::Header::new_gnu();
    header.set_path(path).map_err(ArchiveError::Io)?;
    header.set_size(len);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    tar.append(&header, file).map_err(ArchiveError::Io)?;
    Ok(())
}

fn write_string_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    body: &str,
) -> Result<(), ArchiveError> {
    write_bytes_entry(tar, path, body.as_bytes())
}

/// Local mirror of the archive's `index.json` shape — we don't need
/// the full oci-client `OciImageIndex` because we only ever read the
/// per-manifest annotations + digests off it.
#[derive(Debug, Deserialize)]
struct ArchiveIndex {
    manifests: Vec<ArchiveManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ArchiveManifestEntry {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: i64,
    #[serde(default)]
    annotations: Option<std::collections::BTreeMap<String, String>>,
}

#[cfg(test)]
mod tests;
