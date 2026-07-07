//! On-disk [OCI image layout][spec].
//!
//! Layout v1.0.0:
//!
//! ```text
//! <root>/
//!   oci-layout             {"imageLayoutVersion": "1.0.0"}
//!   index.json             OciImageIndex — references manifests, optionally tagged
//!                          via the `org.opencontainers.image.ref.name` annotation
//!   blobs/sha256/<hex>     content-addressable blob store
//! ```
//!
//! Layouts are interoperable with `crane`, `skopeo`, and any other consumer of the
//! spec. UMF uses this format both for the local registry cache and as the staging
//! area that [`super::client::RegistryClient::pull`] writes into and
//! [`super::client::RegistryClient::push`] reads from.
//!
//! [spec]: https://github.com/opencontainers/image-spec/blob/main/image-layout.md

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use oci_client::manifest::{
    ImageIndexEntry, OCI_IMAGE_MEDIA_TYPE, OciImageIndex, OciImageManifest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::registry::error::RegistryError;
use crate::registry::referrers::ReferrerDescriptor;

/// The supported OCI image layout version.
///
/// Per the spec this is the only currently-defined value.
pub const IMAGE_LAYOUT_VERSION: &str = "1.0.0";

/// Annotation key used to associate a tag with a manifest descriptor in `index.json`.
///
/// Defined by the OCI image spec; honoured by `crane`, `skopeo`, and other tools.
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

const OCI_LAYOUT_FILE: &str = "oci-layout";
const INDEX_JSON_FILE: &str = "index.json";
const BLOBS_DIR: &str = "blobs";
const SHA256_ALGO: &str = "sha256";
/// Non-spec sidecar caches under `cache/`, kept outside `blobs/` so OCI-layout
/// consumers (skopeo, crane) ignore them and `prune_blobs` (which only walks
/// `blobs/sha256/`) never touches them. Each is content-addressed and GC'd
/// separately: `cache/erofs/` holds erofs-encoded lower layers;
/// `cache/blocks/` holds bootable disk images projected by `umf compile`.
const CACHE_DIR: &str = "cache";
const EROFS_CACHE_DIR: &str = "erofs";
const BLOCK_CACHE_DIR: &str = "blocks";

/// Required content of the `oci-layout` marker file at the layout root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayoutMarker {
    /// Spec version. Currently the only legal value is `"1.0.0"`.
    #[serde(rename = "imageLayoutVersion")]
    pub image_layout_version: String,
}

impl Default for LayoutMarker {
    fn default() -> Self {
        Self {
            image_layout_version: IMAGE_LAYOUT_VERSION.to_string(),
        }
    }
}

/// An on-disk OCI image layout. Operations are synchronous because every byte
/// passes through the local filesystem — no need for `async`.
#[derive(Debug, Clone)]
pub struct ImageLayout {
    root: PathBuf,
}

impl ImageLayout {
    /// Initialise a fresh layout at `root`.
    ///
    /// Creates the directory hierarchy, writes the `oci-layout` marker, and
    /// writes an empty `index.json`. Idempotent against an already-initialised
    /// layout: if `root` already contains one it is opened and returned unchanged.
    pub fn init(root: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join(BLOBS_DIR).join(SHA256_ALGO))?;
        fs::create_dir_all(root.join(CACHE_DIR).join(EROFS_CACHE_DIR))?;

        let marker_path = root.join(OCI_LAYOUT_FILE);
        if !marker_path.exists() {
            atomic_write_json(&marker_path, &LayoutMarker::default())?;
        }

        let index_path = root.join(INDEX_JSON_FILE);
        if !index_path.exists() {
            atomic_write_json(&index_path, &empty_index())?;
        }

        Self::open(root)
    }

    /// Open an existing layout at `root`, verifying the `oci-layout` marker.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let root = root.as_ref().to_path_buf();
        let marker_path = root.join(OCI_LAYOUT_FILE);
        let bytes = fs::read(&marker_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RegistryError::InvalidLayout(format!(
                    "missing {OCI_LAYOUT_FILE} at {}",
                    root.display()
                ))
            } else {
                RegistryError::Io(e)
            }
        })?;
        let marker: LayoutMarker = serde_json::from_slice(&bytes)?;
        if marker.image_layout_version != IMAGE_LAYOUT_VERSION {
            return Err(RegistryError::InvalidLayout(format!(
                "unsupported imageLayoutVersion {:?} (want {IMAGE_LAYOUT_VERSION})",
                marker.image_layout_version
            )));
        }
        Ok(Self { root })
    }

    /// Filesystem root of the layout.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Filesystem path to the blob with `digest`. The file may or may not exist.
    ///
    /// `digest` must be in the spec form `algo:hex`; only `sha256` is supported.
    pub fn blob_path(&self, digest: &str) -> Result<PathBuf, RegistryError> {
        // `split_digest` enforces `algo == sha256` + a hex-only `hex` half,
        // so the joined path stays inside `blobs/sha256/`.
        let (algo, hex) = split_digest(digest)?;
        Ok(self.root.join(BLOBS_DIR).join(algo).join(hex))
    }

    /// Filesystem path to the erofs-encoded form of the layer whose compressed
    /// blob digest (the OCI layer `digest`) is `layer_digest`. The file may or
    /// may not exist; [`crate::erofs::ensure_layer_erofs`] produces it.
    ///
    /// Content-addressed on the **layer digest** — the value the blob store
    /// verifies on pull — not the manifest's `diff_id`, which is untrusted until
    /// the tar is decompressed. Keying on the verified digest means a lying
    /// `diff_id` cannot make one image's build reuse another's cached erofs (the
    /// trade-off is no cross-codec dedup, which umf doesn't rely on). A base
    /// layer shared across images still resolves to one erofs file.
    /// `layer_digest` must be in spec form `sha256:hex`.
    pub fn erofs_cache_path(&self, layer_digest: &str) -> Result<PathBuf, RegistryError> {
        let (_algo, hex) = split_digest(layer_digest)?;
        Ok(self
            .root
            .join(CACHE_DIR)
            .join(EROFS_CACHE_DIR)
            .join(format!("{hex}.erofs")))
    }

    /// Filesystem path to the bootable disk image projected from the OS image
    /// whose manifest digest is `image_digest`, for the projection `variant`
    /// (a hex fingerprint of the compile parameters — geometry, filesystem,
    /// bootloader, …). The file may or may not exist; `umf compile` produces
    /// it. Content-addressed on the source image, so the same image + the same
    /// parameters resolve to one cached block.
    ///
    /// Like [`Self::erofs_cache_path`], a non-spec sidecar under `cache/` —
    /// never pushed and never part of the OCI index.
    pub fn block_cache_path(
        &self,
        image_digest: &str,
        variant: &str,
    ) -> Result<PathBuf, RegistryError> {
        let (_algo, hex) = split_digest(image_digest)?;
        // `variant` is joined into the path, so gate it to hex like the digest
        // half — keeps traversal and absolute paths un-representable.
        if variant.is_empty() || !variant.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RegistryError::MalformedDigest(format!(
                "block-cache variant must be non-empty hex, got {variant:?}"
            )));
        }
        Ok(self
            .root
            .join(CACHE_DIR)
            .join(BLOCK_CACHE_DIR)
            .join(hex)
            .join(format!("{variant}.img")))
    }

    /// Whether the layout contains a blob with `digest`.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).map(|p| p.exists()).unwrap_or(false)
    }

    /// Read a blob, verifying its sha256 matches `digest`.
    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>, RegistryError> {
        let path = self.blob_path(digest)?;
        let bytes = fs::read(path)?;
        let computed = sha256_digest(&bytes);
        if computed != digest {
            return Err(RegistryError::DigestMismatch {
                expected: digest.to_string(),
                found: computed,
            });
        }
        Ok(bytes)
    }

    /// Write `data` as a blob. The digest is computed and returned.
    ///
    /// If a blob with the same digest already exists, this is a no-op (content
    /// addressing — equal digest implies equal content).
    pub fn write_blob(&self, data: &[u8]) -> Result<String, RegistryError> {
        let digest = sha256_digest(data);
        self.write_blob_with_digest(data, &digest)?;
        Ok(digest)
    }

    /// Write `data` as a blob, verifying its sha256 matches `expected_digest`.
    ///
    /// Use this when the digest is known up front (taken from a manifest
    /// descriptor). The blob is written atomically: bytes go to a temp file
    /// alongside the target, are synced, then renamed into place.
    pub fn write_blob_with_digest(
        &self,
        data: &[u8],
        expected_digest: &str,
    ) -> Result<(), RegistryError> {
        let computed = sha256_digest(data);
        if computed != expected_digest {
            return Err(RegistryError::DigestMismatch {
                expected: expected_digest.to_string(),
                found: computed,
            });
        }
        let dest = self.blob_path(expected_digest)?;
        if dest.exists() {
            return Ok(());
        }
        atomic_write_bytes(&dest, data)
    }

    /// Read and parse `index.json`.
    pub fn read_index(&self) -> Result<OciImageIndex, RegistryError> {
        let path = self.root.join(INDEX_JSON_FILE);
        let bytes = fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Replace `index.json` atomically.
    pub fn write_index(&self, index: &OciImageIndex) -> Result<(), RegistryError> {
        atomic_write_json(&self.root.join(INDEX_JSON_FILE), index)
    }

    /// Insert or replace the entry whose `org.opencontainers.image.ref.name`
    /// annotation matches `ref_name`. Used by the registry client to record
    /// `tag → manifest digest` associations after a pull, and looked up by
    /// `push` to find the manifest to upload.
    pub fn upsert_ref(
        &self,
        ref_name: &str,
        mut entry: ImageIndexEntry,
    ) -> Result<(), RegistryError> {
        let mut index = self.read_index()?;
        let annotations = entry.annotations.get_or_insert_with(Default::default);
        annotations.insert(REF_NAME_ANNOTATION.to_string(), ref_name.to_string());

        index
            .manifests
            .retain(|e| ref_name_of(e).as_deref() != Some(ref_name));
        index.manifests.push(entry);

        self.write_index(&index)
    }

    /// Find the entry whose `ref.name` annotation matches `ref_name`.
    pub fn lookup_ref(&self, ref_name: &str) -> Result<Option<ImageIndexEntry>, RegistryError> {
        let index = self.read_index()?;
        Ok(index
            .manifests
            .into_iter()
            .find(|e| ref_name_of(e).as_deref() == Some(ref_name)))
    }

    /// List every (ref_name, entry) pair currently in `index.json`.
    /// Order matches the on-disk index — what `umf images` displays.
    pub fn list_refs(&self) -> Result<Vec<(String, ImageIndexEntry)>, RegistryError> {
        let index = self.read_index()?;
        Ok(index
            .manifests
            .into_iter()
            .filter_map(|e| ref_name_of(&e).map(|n| (n, e)))
            .collect())
    }

    /// Drop the entry whose `ref.name` matches `ref_name`. Returns
    /// `true` when something was removed, `false` when the ref
    /// wasn't present. Blobs are untouched — use
    /// [`Self::prune_blobs`] after one or more removes to GC any
    /// blobs no longer reachable from the index.
    pub fn remove_ref(&self, ref_name: &str) -> Result<bool, RegistryError> {
        let mut index = self.read_index()?;
        let before = index.manifests.len();
        index
            .manifests
            .retain(|e| ref_name_of(e).as_deref() != Some(ref_name));
        let removed = before != index.manifests.len();
        if removed {
            self.write_index(&index)?;
        }
        Ok(removed)
    }

    /// Insert `entry` into `index.json` **without** a ref-name annotation.
    ///
    /// Untagged entries make digest-addressed manifests — referrer artifacts,
    /// chiefly — reachability roots for [`Self::prune_blobs`] and
    /// discoverable by [`Self::list_referrers`], without surfacing in
    /// [`Self::list_refs`] (and therefore `umf images`). Replaces an existing
    /// *untagged* entry with the same digest; tagged entries are never
    /// touched.
    pub fn upsert_untagged(&self, entry: ImageIndexEntry) -> Result<(), RegistryError> {
        let mut index = self.read_index()?;
        index
            .manifests
            .retain(|e| !(ref_name_of(e).is_none() && e.digest == entry.digest));
        index.manifests.push(entry);
        self.write_index(&index)
    }

    /// List the referrers of `subject_digest` recorded in this layout: every
    /// image manifest in `index.json` — tagged or untagged — whose `subject`
    /// descriptor points at `subject_digest`, optionally filtered by
    /// `artifact_type`.
    ///
    /// Descriptors carry the referrer's `artifactType` and annotations — the
    /// same shape a registry's referrers API returns, so a local listing and
    /// a remote one are interchangeable. An entry whose manifest blob is
    /// missing or unparseable is skipped, mirroring the prune walk's
    /// tolerance: the index is the root set, not a guarantee of blob health.
    pub fn list_referrers(
        &self,
        subject_digest: &str,
        artifact_type: Option<&str>,
    ) -> Result<Vec<ReferrerDescriptor>, RegistryError> {
        let index = self.read_index()?;
        let mut referrers = Vec::new();
        for entry in &index.manifests {
            if entry.media_type != OCI_IMAGE_MEDIA_TYPE {
                continue;
            }
            let Ok(bytes) = self.read_blob(&entry.digest) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<OciImageManifest>(&bytes) else {
                continue;
            };
            if manifest.subject.as_ref().map(|s| s.digest.as_str()) != Some(subject_digest) {
                continue;
            }
            if let Some(filter) = artifact_type
                && manifest.artifact_type.as_deref() != Some(filter)
            {
                continue;
            }
            referrers.push(ReferrerDescriptor {
                media_type: entry.media_type.clone(),
                digest: entry.digest.clone(),
                size: entry.size,
                artifact_type: manifest.artifact_type,
                annotations: manifest.annotations,
            });
        }
        Ok(referrers)
    }

    /// Walk every manifest currently in the index, recurse through
    /// referenced manifests + configs + layers, and delete any
    /// `blobs/sha256/<hex>` file that isn't in the reachable set.
    /// Returns `(blobs_removed, bytes_freed)`.
    ///
    /// Best-effort I/O: a file that fails to delete is logged and
    /// the rest of the prune continues. Index updates are not made
    /// here — the index is what defines reachability.
    ///
    /// If a manifest referenced by the index is on disk but corrupt (its bytes
    /// don't match its digest), the reachability walk can't enumerate its
    /// config + layers, so the prune aborts with [`RegistryError::DigestMismatch`]
    /// rather than risk deleting those still-referenced blobs. A
    /// genuinely-absent referenced manifest is a no-op leaf and does not abort.
    pub fn prune_blobs(&self) -> Result<(usize, u64), RegistryError> {
        use std::collections::HashSet;

        let index = self.read_index()?;
        let mut reachable: HashSet<String> = HashSet::new();
        for entry in &index.manifests {
            self.collect_reachable(&entry.digest, &mut reachable)?;
        }

        let blobs_dir = self.root.join(BLOBS_DIR).join(SHA256_ALGO);
        prune_dir(
            &blobs_dir,
            PruneKind::File,
            // Each `blobs/sha256/<hex>` file's digest is `sha256:<hex>`.
            |name| name.to_str().map(|n| format!("{SHA256_ALGO}:{n}")),
            |digest| reachable.contains(digest),
        )
    }

    /// GC the erofs lower-layer cache: delete every
    /// `cache/erofs/<hex>.erofs` whose diff_id (`sha256:<hex>`) is no
    /// longer reachable from any manifest in the index. Returns
    /// `(files_removed, bytes_freed)`.
    ///
    /// Companion to [`Self::prune_blobs`] — that one GCs `blobs/sha256/`
    /// by blob digest; this one GCs the erofs sidecar by layer diff_id.
    /// Best-effort I/O: a file that won't delete is skipped.
    pub fn prune_erofs_cache(&self) -> Result<(usize, u64), RegistryError> {
        use std::collections::HashSet;

        let index = self.read_index()?;
        let mut reachable: HashSet<String> = HashSet::new();
        for entry in &index.manifests {
            self.collect_reachable_diff_ids(&entry.digest, &mut reachable)?;
        }

        let erofs_dir = self.root.join(CACHE_DIR).join(EROFS_CACHE_DIR);
        prune_dir(
            &erofs_dir,
            PruneKind::File,
            // `cache/erofs/<hex>.erofs` → diff_id `sha256:<hex>`; non-`.erofs`
            // entries return None and are skipped.
            |name| {
                name.to_str()
                    .and_then(|n| n.strip_suffix(".erofs"))
                    .map(|hex| format!("{SHA256_ALGO}:{hex}"))
            },
            |diff_id| reachable.contains(diff_id),
        )
    }

    /// GC the block cache: delete every `cache/blocks/<hex>/` subtree whose
    /// source image manifest (`sha256:<hex>`) is no longer present in the
    /// index. Returns `(subtrees_removed, bytes_freed)`. Blocks are regenerable
    /// (`umf compile` recreates them on demand), so pruning them alongside
    /// images is always safe.
    ///
    /// Unlike [`Self::prune_erofs_cache`] (which keys on layer diff_ids), a
    /// block is keyed by the *manifest* digest the operator compiled, so
    /// liveness is the set of manifest digests in `index.json`. (Multi-arch
    /// child manifests aren't walked yet — the single-platform case covers
    /// today's `umf compile`.)
    pub fn prune_block_cache(&self) -> Result<(usize, u64), RegistryError> {
        use std::collections::HashSet;

        let index = self.read_index()?;
        let live: HashSet<String> = index.manifests.iter().map(|m| m.digest.clone()).collect();

        let blocks_dir = self.root.join(CACHE_DIR).join(BLOCK_CACHE_DIR);
        prune_dir(
            &blocks_dir,
            PruneKind::Dir,
            // Each `cache/blocks/<hex>/` subtree keys on manifest `sha256:<hex>`.
            |name| name.to_str().map(|n| format!("{SHA256_ALGO}:{n}")),
            |digest| live.contains(digest),
        )
    }
}

impl ImageLayout {
    /// Walk a manifest (or index) blob's transitive reachable set —
    /// config + layers + (for index) child manifests. Adds every visited
    /// digest to `out`, including the starting one. Shared by
    /// [`Self::prune_blobs`] and the archive exporter ([`crate::archive`]) so
    /// the two GC walks can't drift.
    ///
    /// Error handling is deliberately asymmetric to keep `prune_blobs` from
    /// GC'ing blobs that are still referenced. The `digest`
    /// passed here is reachable *by construction* — it comes from `index.json`
    /// or a parent manifest — so we must never silently drop its subtree:
    ///
    /// - **Genuinely absent** ([`RegistryError::Io`] with
    ///   [`std::io::ErrorKind::NotFound`]) → leaf. There is no blob on disk, so
    ///   there are no children to reach and nothing to prune under it.
    /// - **Present but corrupt** ([`RegistryError::DigestMismatch`]) → the blob
    ///   is on disk but its bytes don't hash to `digest`, so we can't trust
    ///   (or even parse) its config/layer references. Returning the error
    ///   aborts the prune rather than treating the manifest as a leaf — which
    ///   would mark its still-referenced config + layers unreachable and delete
    ///   them.
    /// - **Malformed digest / other I/O** (any other [`RegistryError`]) →
    ///   propagated for the same reason: a digest we can't resolve is not safe
    ///   to assume childless.
    ///
    /// A blob that reads back *with a matching digest* but fails to parse as an
    /// OCI manifest is a real leaf (a layer / config / unknown artifact), not
    /// corruption, and is treated as such.
    pub(crate) fn collect_reachable(
        &self,
        digest: &str,
        out: &mut std::collections::HashSet<String>,
    ) -> Result<(), RegistryError> {
        if !out.insert(digest.to_string()) {
            return Ok(()); // already walked
        }
        let bytes = match self.read_blob(digest) {
            Ok(b) => b,
            // Genuinely absent: no blob, no children — a safe leaf.
            Err(RegistryError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            // Corrupt / unresolvable but referenced by the index: do NOT treat
            // as a leaf (that would prune its config + layers). Abort instead.
            Err(e) => return Err(e),
        };
        let Ok(parsed) = serde_json::from_slice::<oci_client::manifest::OciManifest>(&bytes) else {
            return Ok(()); // not a manifest — leaf blob (layer / config / unknown)
        };
        match parsed {
            oci_client::manifest::OciManifest::Image(m) => {
                out.insert(m.config.digest.clone());
                for layer in &m.layers {
                    out.insert(layer.digest.clone());
                }
            }
            oci_client::manifest::OciManifest::ImageIndex(index) => {
                for child in &index.manifests {
                    self.collect_reachable(&child.digest, out)?;
                }
            }
        }
        Ok(())
    }

    /// Total on-disk size, in bytes, of every unique blob reachable from the
    /// manifest `digest`: the manifest itself, its config and layers, and for a
    /// multi-arch image index every child manifest's subtree, de-duped by
    /// digest.
    ///
    /// Sums the actual blob *file* sizes — it stats them, never reading layer
    /// bytes into memory — and reuses the same [`Self::collect_reachable`] walk
    /// [`Self::prune_blobs`] uses, so the figure matches what pruning an
    /// otherwise-unreferenced image would free. This is the image's real
    /// footprint, not the top-level manifest's own byte length (which is what
    /// the ref's `index.json` entry records — only a few KiB for an index).
    ///
    /// Absent blobs contribute 0; a corrupt referenced manifest propagates the
    /// error (the same reachability contract as [`Self::collect_reachable`]).
    pub fn image_disk_size(&self, digest: &str) -> Result<u64, RegistryError> {
        use std::collections::HashSet;

        let mut reachable: HashSet<String> = HashSet::new();
        self.collect_reachable(digest, &mut reachable)?;

        let mut total: u64 = 0;
        for blob in &reachable {
            if let Ok(path) = self.blob_path(blob)
                && let Ok(meta) = std::fs::metadata(&path)
            {
                total += meta.len();
            }
        }
        Ok(total)
    }

    /// Walk a manifest (or index) and collect the **diff_ids** of every
    /// reachable image's layers, read from each image config's
    /// `rootfs.diff_ids`. Drives [`Self::prune_erofs_cache`], whose cache
    /// is keyed on diff_id (not blob digest). Missing/garbled blobs are
    /// treated as leaves.
    fn collect_reachable_diff_ids(
        &self,
        digest: &str,
        out: &mut std::collections::HashSet<String>,
    ) -> Result<(), RegistryError> {
        let Ok(bytes) = self.read_blob(digest) else {
            return Ok(());
        };
        let Ok(parsed) = serde_json::from_slice::<oci_client::manifest::OciManifest>(&bytes) else {
            return Ok(());
        };
        match parsed {
            oci_client::manifest::OciManifest::Image(m) => {
                if let Ok(cfg_bytes) = self.read_blob(&m.config.digest)
                    && let Ok(cfg) = serde_json::from_slice::<serde_json::Value>(&cfg_bytes)
                    && let Some(ids) = cfg
                        .get("rootfs")
                        .and_then(|r| r.get("diff_ids"))
                        .and_then(|d| d.as_array())
                {
                    for id in ids.iter().filter_map(serde_json::Value::as_str) {
                        out.insert(id.to_string());
                    }
                }
            }
            oci_client::manifest::OciManifest::ImageIndex(index) => {
                for child in &index.manifests {
                    self.collect_reachable_diff_ids(&child.digest, out)?;
                }
            }
        }
        Ok(())
    }
}

/// Compute the sha256 of `data` and return it in OCI form (`sha256:<lower-hex>`).
pub fn sha256_digest(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{SHA256_ALGO}:{}", hex::encode(hasher.finalize()))
}

/// Split a spec-form digest `algo:hex` into its parts, enforcing the only
/// supported algorithm (`sha256`).
///
/// The `hex` half is required to be non-empty and **purely hexadecimal**.
/// [`ImageLayout::blob_path`], [`ImageLayout::erofs_cache_path`], and
/// [`ImageLayout::block_cache_path`] all join `hex` straight into a
/// filesystem path, and digests arrive from untrusted image
/// manifests/configs — so a `hex` containing `/`, `.`, `\`, or a second
/// `:` would let a malicious `diff_id` (e.g. `sha256:../../etc/x` or
/// `sha256:/abs/path`) escape the layout via `PathBuf::join`. Restricting
/// `hex` to `[0-9a-fA-F]` makes traversal and absolute paths
/// un-representable; the charset gate is the security boundary, so it lives
/// here at the single chokepoint. The algorithm check lives here too — a
/// non-`sha256` algorithm yields [`RegistryError::MalformedDigest`].
pub(crate) fn split_digest(digest: &str) -> Result<(&str, &str), RegistryError> {
    let (algo, hex) = digest
        .split_once(':')
        .filter(|(algo, hex)| {
            !algo.is_empty() && !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .ok_or_else(|| RegistryError::MalformedDigest(digest.to_string()))?;
    if algo != SHA256_ALGO {
        return Err(RegistryError::MalformedDigest(format!(
            "unsupported digest algorithm {algo:?}"
        )));
    }
    Ok((algo, hex))
}

/// Whether a [`prune_dir`] entry is a single file or a subtree — controls
/// the directory-entry filter, the size accounting, and the removal call.
#[derive(Clone, Copy)]
enum PruneKind {
    /// `blobs/sha256/<hex>` / `cache/erofs/<hex>.erofs` — a file. Size is the
    /// file length; removal is `remove_file`. No file-type filter (the
    /// original loops removed whatever was at the path).
    File,
    /// `cache/blocks/<hex>/` — a subtree. Non-directory entries are skipped;
    /// size is the recursive sum; removal is `remove_dir_all`.
    Dir,
}

/// Shared GC skeleton for the three prune loops: read `dir`, map each entry's
/// filename to a liveness key via `key_of` (returning `None` skips the entry),
/// keep entries whose key `is_live`, and best-effort remove the rest while
/// tallying freed bytes. Returns `(removed, bytes_freed)`.
///
/// A missing `dir` is a no-op `(0, 0)` (nothing has been cached yet). I/O is
/// best-effort: an entry that won't delete is left in place and not counted,
/// matching the prior per-loop behavior.
fn prune_dir(
    dir: &Path,
    kind: PruneKind,
    key_of: impl Fn(&std::ffi::OsStr) -> Option<String>,
    is_live: impl Fn(&str) -> bool,
) -> Result<(usize, u64), RegistryError> {
    if !dir.is_dir() {
        return Ok((0, 0));
    }
    let mut removed = 0_usize;
    let mut bytes_freed = 0_u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        // `Dir` kind filters to subdirectories (and propagates a file-type
        // error, as the block-cache loop did); `File` kind takes any entry.
        if matches!(kind, PruneKind::Dir) && !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(key) = key_of(&entry.file_name()) else {
            continue;
        };
        if is_live(&key) {
            continue;
        }
        match kind {
            PruneKind::File => {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                    bytes_freed += size;
                }
            }
            PruneKind::Dir => {
                let size = dir_size(&entry.path());
                if fs::remove_dir_all(entry.path()).is_ok() {
                    removed += 1;
                    bytes_freed += size;
                }
            }
        }
    }
    Ok((removed, bytes_freed))
}

/// Sum the byte size of every regular file under `path`, recursively.
/// Best-effort: unreadable entries contribute 0.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(rd) = fs::read_dir(path) else {
        return 0;
    };
    for entry in rd.flatten() {
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => total += dir_size(&entry.path()),
            Ok(_) => total += entry.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => {}
        }
    }
    total
}

fn ref_name_of(entry: &ImageIndexEntry) -> Option<String> {
    entry
        .annotations
        .as_ref()
        .and_then(|a| a.get(REF_NAME_ANNOTATION).cloned())
}

fn empty_index() -> OciImageIndex {
    OciImageIndex {
        schema_version: 2,
        media_type: Some(oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        manifests: Vec::new(),
        artifact_type: None,
        annotations: None,
    }
}

fn atomic_write_bytes(dest: &Path, data: &[u8]) -> Result<(), RegistryError> {
    let parent = dest.parent().ok_or_else(|| {
        RegistryError::InvalidLayout(format!("blob path has no parent: {}", dest.display()))
    })?;
    fs::create_dir_all(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| RegistryError::Io(e.error))?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(dest: &Path, value: &T) -> Result<(), RegistryError> {
    let parent = dest.parent().ok_or_else(|| {
        RegistryError::InvalidLayout(format!("json path has no parent: {}", dest.display()))
    })?;
    fs::create_dir_all(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut tmp, value)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| RegistryError::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests;
