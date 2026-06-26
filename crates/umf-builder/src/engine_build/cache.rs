//! Content-addressable step cache for RUN/ADD steps.
//!
//! The key is a SHA-256 over the cumulative parent state plus a canonical
//! rendering of the directive's inputs; the entry is the digest of the
//! layer blob we previously emitted into the layout.
//!
//! On a no-change rebuild every step hits the cache, so we skip both the
//! container execution and the layer-packaging tarball. The acceptance
//! criterion calls this out explicitly.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use umf_oci::image::LayerSource;
use umf_oci::registry::ImageLayout;

use super::EngineBuildError;
use super::state::BuildState;

#[derive(Debug)]
pub(crate) struct StepCache {
    /// `<layout_root>/umf-engine-cache/` — created lazily on first put.
    root: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CachedStep {
    pub(crate) blob_digest: String,
    pub(crate) diff_id: String,
    pub(crate) media_type: String,
    pub(crate) history_line: String,
}

impl StepCache {
    pub(crate) fn for_layout(layout: &ImageLayout) -> Self {
        Self {
            root: layout.root().join("umf-engine-cache"),
        }
    }

    pub(crate) fn lookup(&self, key: &str) -> Option<CachedStep> {
        let path = self.root.join(format!("{key}.json"));
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub(crate) fn store(&self, key: &str, entry: &CachedStep) -> Result<(), EngineBuildError> {
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(entry)?;
        std::fs::write(self.root.join(format!("{key}.json")), bytes)?;
        Ok(())
    }
}

/// Cumulative "parent state" hash — used as the cache key's prefix so
/// a step's cache entry is only valid against the exact layer stack it
/// was produced under.
pub(crate) fn parent_state_hash(state: &BuildState) -> String {
    let mut hasher = Sha256::new();
    for layer in &state.base_layers {
        hasher.update(layer.diff_id.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"--\n");
    for layer in &state.new_layers {
        hasher.update(layer.diff_id.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Compute a deterministic content digest for a local-path `ADD`
/// source — walks the tree in lexicographic order, hashing each
/// entry's relative path + permissions + content.
pub(crate) fn add_source_digest(src: &Path) -> Result<String, EngineBuildError> {
    let mut hasher = Sha256::new();
    if src.is_file() {
        let bytes = std::fs::read(src)?;
        let mode = std::fs::metadata(src)?.permissions().mode() & 0o7777;
        hasher.update(b"file:");
        hasher.update(
            src.file_name()
                .map_or(b"" as &[u8], |s| s.as_encoded_bytes()),
        );
        hasher.update(b"\n");
        hasher.update(format!("mode:{mode:o}\n").as_bytes());
        hasher.update(&bytes);
        return Ok(hex::encode(hasher.finalize()));
    }
    // Directory: sorted walk.
    let mut entries: Vec<walkdir::DirEntry> = walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(Result::ok)
        .collect();
    // Sort by full path: the authoritative order for the content hash. (No
    // WalkDir::sort_by_file_name — it only orders within each directory and is
    // redundant once we sort the collected entries by full path here.)
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    for entry in entries {
        let rel = entry
            .path()
            .strip_prefix(src)
            .unwrap_or(entry.path())
            .to_path_buf();
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\n");
        // `symlink_metadata` (lstat) so a symlink's own mode is hashed
        // rather than the target's, and it yields `std::io::Error` which
        // folds cleanly into `EngineBuildError` (unlike `walkdir::Error`).
        let mode = entry.path().symlink_metadata()?.permissions().mode() & 0o7777;
        hasher.update(format!("mode:{mode:o}\n").as_bytes());
        if entry.file_type().is_file() {
            let bytes = std::fs::read(entry.path())?;
            hasher.update(&bytes);
        } else if entry.file_type().is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            hasher.update(b"->");
            hasher.update(target.to_string_lossy().as_bytes());
        }
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Build a cache key for a RUN step.
///
/// Folds in the target architecture, the layer-compression codec, the
/// parent diff-id chain, argv, env, cwd, user, and the referenced-secret
/// IDs. The codec is hashed so a layout shared across `--compression`
/// codecs never adopts a layer of the other encoding. The architecture is hashed
/// so a layout reused across `--platform` arches can't cross-contaminate
/// (an `amd64` upper must never be adopted for an `arm64` build of the
/// same recipe). Secret *content* is never hashed, so an unchanged recipe
/// with a rotated secret value still gets a cache hit; changing *which*
/// secret a RUN references does bust the cache.
#[allow(clippy::too_many_arguments)] // hash inputs are exactly these
pub(crate) fn run_cache_key(
    arch: &str,
    codec: &str,
    parent: &str,
    argv: &[String],
    env: &[String],
    cwd: Option<&str>,
    user: Option<&str>,
    secret_ids: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"RUN\n");
    hasher.update(b"arch:");
    hasher.update(arch.as_bytes());
    hasher.update(b"\ncodec:");
    hasher.update(codec.as_bytes());
    hasher.update(b"\n");
    hasher.update(parent.as_bytes());
    hasher.update(b"\n");
    for a in argv {
        hasher.update(a.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\nenv:\n");
    for e in env {
        hasher.update(e.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"cwd:");
    hasher.update(cwd.unwrap_or("").as_bytes());
    hasher.update(b"\nuser:");
    hasher.update(user.unwrap_or("").as_bytes());
    hasher.update(b"\nsecrets:\n");
    // Sort so cache key is independent of declaration order.
    let mut ids = secret_ids.to_vec();
    ids.sort();
    for id in &ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Build a cache key for an ADD step.
///
/// The target architecture and layer-compression codec are folded in
/// alongside the source-content digest and destination so a layout shared
/// across `--platform` arches (or `--compression` codecs) keeps the
/// produced ADD layers distinct.
pub(crate) fn add_cache_key(
    arch: &str,
    codec: &str,
    parent: &str,
    src_digest: &str,
    dst: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ADD\n");
    hasher.update(b"arch:");
    hasher.update(arch.as_bytes());
    hasher.update(b"\ncodec:");
    hasher.update(codec.as_bytes());
    hasher.update(b"\n");
    hasher.update(parent.as_bytes());
    hasher.update(b"\n");
    hasher.update(src_digest.as_bytes());
    hasher.update(b"\n");
    hasher.update(dst.as_bytes());
    hex::encode(hasher.finalize())
}

/// Cache key for `ADD <url>`. Keyed on the *fetched payload's* sha256 —
/// the URL is re-fetched every build (docker semantics), and the digest
/// decides whether the layer is reused, so an unchanged remote hits and a
/// silently-changed one busts.
pub(crate) fn url_add_cache_key(
    codec: &str,
    parent: &str,
    payload_digest: &str,
    dst: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ADD-URL\ncodec:");
    hasher.update(codec.as_bytes());
    hasher.update(b"\n");
    hasher.update(parent.as_bytes());
    hasher.update(b"\n");
    hasher.update(payload_digest.as_bytes());
    hasher.update(b"\n");
    hasher.update(dst.as_bytes());
    hex::encode(hasher.finalize())
}

/// Cache key for a bare `ADD <oci-ref>`. Keyed on the resolved image's
/// manifest digest plus destination (and codec), so a retagged upstream
/// image busts the cache through the digest rather than the tag.
pub(crate) fn oci_image_add_cache_key(
    codec: &str,
    parent: &str,
    manifest_digest: &str,
    dst: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ADD-OCI\ncodec:");
    hasher.update(codec.as_bytes());
    hasher.update(b"\n");
    hasher.update(parent.as_bytes());
    hasher.update(b"\n");
    hasher.update(manifest_digest.as_bytes());
    hasher.update(b"\n");
    hasher.update(dst.as_bytes());
    hex::encode(hasher.finalize())
}

/// Cache key for `ADD --from=<stage>`. Uses the producer's manifest
/// digest so any change upstream busts the cache.
pub(crate) fn cross_stage_add_cache_key(
    codec: &str,
    parent: &str,
    producer_manifest_digest: &str,
    src_path: &str,
    dst: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ADD-FROM\ncodec:");
    hasher.update(codec.as_bytes());
    hasher.update(b"\n");
    hasher.update(parent.as_bytes());
    hasher.update(b"\n");
    hasher.update(producer_manifest_digest.as_bytes());
    hasher.update(b"\n");
    hasher.update(src_path.as_bytes());
    hasher.update(b"\n");
    hasher.update(dst.as_bytes());
    hex::encode(hasher.finalize())
}

/// Reconstruct a [`LayerSource`] from a [`CachedStep`] entry by reading
/// the blob back from the layout. Returns `Ok(None)` if the blob is
/// missing (cache entry is dead and the step must be re-executed).
pub(crate) fn layer_from_cache(
    layout: &ImageLayout,
    entry: &CachedStep,
) -> Result<Option<LayerSource>, EngineBuildError> {
    if !layout.has_blob(&entry.blob_digest) {
        return Ok(None);
    }
    let bytes = layout.read_blob(&entry.blob_digest)?;
    Ok(Some(LayerSource {
        data: Bytes::from(bytes),
        media_type: entry.media_type.clone(),
        diff_id: entry.diff_id.clone(),
    }))
}

#[cfg(test)]
mod tests;
