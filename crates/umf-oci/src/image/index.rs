//! OCI image-index (multi-arch) authoring + per-arch selection.
//!
//! The producer-side counterpart to the index *consume* path in
//! [`crate::registry::RegistryClient::pull`]: given a set of already-built
//! per-arch image manifests (each emitted by [`super::emit_image`] and already
//! present in the layout), assemble a valid
//! `application/vnd.oci.image.index.v1+json` whose `manifests[].platform` is
//! populated, write it into the layout, and register it under a ref name. The
//! emitted index is immediately consumable by
//! [`crate::registry::RegistryClient::push`] (which already pushes each child
//! manifest tree first) and by external tooling (`skopeo inspect`, `crane`).
//!
//! ## Reproducibility
//!
//! Byte reproducibility is a hard `umf-oci` invariant. For the index it is
//! maintained by:
//!
//! * **Deterministic child order** — [`emit_index`] sorts children by
//!   `(os, architecture, variant, digest)` before serialising, so the same set
//!   of children in any input order yields the byte-identical index manifest
//!   (and therefore the same index digest).
//! * No implicit timestamps — an index carries none.
//! * `BTreeMap` annotations (none are written here, but the field stays stable).

use oci_client::manifest::{
    ImageIndexEntry, OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE, OciImageIndex, Platform,
};
use oci_spec::image::{Arch, Os};

use crate::registry::ImageLayout;
use crate::registry::error::RegistryError;
use crate::registry::layout::sha256_digest;

/// One child of an image index: an already-built per-arch manifest plus the
/// platform it targets.
///
/// The `manifest_digest` must already resolve to an image manifest in the
/// layout (the typical producer flow runs `umf build --platform=…` once per
/// arch, each writing its manifest + blobs into the same layout, then composes
/// them here). [`emit_index`] reads each referenced manifest blob to learn its
/// byte length for the descriptor `size`, which also doubles as a presence /
/// integrity check.
#[derive(Debug, Clone)]
pub struct IndexChild {
    /// Platform descriptor (`os` / `architecture` / optional `variant`) written
    /// verbatim into the index entry. Drive it from
    /// [`platform_for`] so the strings stay OCI-shaped.
    pub platform: Platform,
    /// Digest of the per-arch image manifest, `sha256:<hex>`.
    pub manifest_digest: String,
}

/// Build a minimal linux platform descriptor for `arch`.
///
/// `arch` is the OCI architecture shorthand (`"amd64"`, `"arm64"`, …) as
/// produced by [`umf_core::architecture::Architecture::oci_arch_string`];
/// unknown values are preserved verbatim via [`Arch::from`] /
/// [`Os::from`] (the `Other(String)` variants), never rejected, so the index
/// stays expressible for arches UMF does not special-case.
#[must_use]
pub fn platform_for(os: &str, arch: &str) -> Platform {
    Platform {
        architecture: Arch::from(arch),
        os: Os::from(os),
        os_version: None,
        os_features: None,
        variant: None,
        features: None,
    }
}

/// Assemble an `application/vnd.oci.image.index.v1+json` from `children`, write
/// it into `layout`, and register it under `ref_name`.
///
/// Each child's manifest must already be present in `layout`; this never pulls.
/// Children are sorted (see the module-level reproducibility note) so the
/// emitted index is byte-for-byte identical for a given child set regardless of
/// input order. Returns the descriptor recorded in `index.json` for the index
/// manifest itself, the same shape [`super::emit_image`] returns for a single
/// image.
///
/// # Errors
/// * [`RegistryError::InvalidLayout`] when `children` is empty (an index with
///   no manifests is legal per spec but useless as a build output, and almost
///   always a caller bug).
/// * [`RegistryError`] (I/O / not-found) when a child manifest digest is absent
///   from the layout.
pub fn emit_index(
    layout: &ImageLayout,
    children: &[IndexChild],
    ref_name: &str,
) -> Result<ImageIndexEntry, RegistryError> {
    if children.is_empty() {
        return Err(RegistryError::InvalidLayout(
            "cannot emit an image index with no child manifests".to_string(),
        ));
    }

    // Build the entries, reading each child manifest blob to learn its size
    // (and prove it is in the layout). The media type is the OCI image-manifest
    // type — these children are single-arch images produced by `emit_image`.
    let mut entries = Vec::with_capacity(children.len());
    for child in children {
        let manifest_bytes = layout.read_blob(&child.manifest_digest)?;
        entries.push(ImageIndexEntry {
            media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
            digest: child.manifest_digest.clone(),
            size: manifest_bytes.len() as i64,
            platform: Some(child.platform.clone()),
            annotations: None,
        });
    }

    // Deterministic order ⇒ reproducible index bytes. Sort by the platform's
    // human-facing identity (os, arch, variant) and break ties on digest so two
    // children that somehow share a platform still order stably.
    entries.sort_by_key(platform_sort_key);

    let index = OciImageIndex {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
        manifests: entries,
        artifact_type: None,
        annotations: None,
    };
    let index_bytes = serde_json::to_vec(&index)?;
    let index_digest = sha256_digest(&index_bytes);
    layout.write_blob_with_digest(&index_bytes, &index_digest)?;

    let entry = ImageIndexEntry {
        media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
        digest: index_digest,
        size: index_bytes.len() as i64,
        platform: None,
        annotations: None,
    };
    layout.upsert_ref(ref_name, entry.clone())?;
    Ok(entry)
}

/// Stable sort key for an index entry: `(os, arch, variant, digest)` rendered
/// as strings. `Arch` / `Os` `Display` is the OCI-canonical spelling, so the
/// key is independent of the in-memory enum order.
fn platform_sort_key(entry: &ImageIndexEntry) -> (String, String, String, String) {
    let (os, arch, variant) = match &entry.platform {
        Some(p) => (
            p.os.to_string(),
            p.architecture.to_string(),
            p.variant.clone().unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    (os, arch, variant, entry.digest.clone())
}

/// Select the child image-manifest digest in an index that matches `arch`
/// (linux only).
///
/// Mirrors the container base-image resolver in
/// `umf-builder::engine_build::base_image`: prefer the variant-less match for
/// the requested arch, then any variant of the same arch. Returns `None` when
/// no child targets the arch (the caller turns that into a clear error rather
/// than silently falling back to the first child, which would consume the
/// wrong architecture).
#[must_use]
pub fn select_manifest_for_arch<'a>(
    index: &'a OciImageIndex,
    arch: &str,
) -> Option<&'a ImageIndexEntry> {
    let want = Arch::from(arch);
    index
        .manifests
        .iter()
        .find(|m| {
            m.platform.as_ref().is_some_and(|p| {
                p.os == Os::Linux
                    && p.architecture == want
                    && p.variant.as_deref().is_none_or(str::is_empty)
            })
        })
        .or_else(|| {
            index.manifests.iter().find(|m| {
                m.platform
                    .as_ref()
                    .is_some_and(|p| p.os == Os::Linux && p.architecture == want)
            })
        })
}

#[cfg(test)]
mod tests;
