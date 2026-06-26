//! OCI 1.1 artifact manifests — `subject` / `artifactType` emission (OCI-4a).
//!
//! An *artifact manifest* (image-spec, "Guidelines for Artifact Usage") is an
//! ordinary image manifest whose `config` descriptor points at the canonical
//! empty-JSON blob (`application/vnd.oci.empty.v1+json`, the two bytes `{}`),
//! whose `artifactType` declares what the artifact is (an SBOM document, a
//! signature envelope, an attestation, …), and whose `layers` carry the
//! artifact's blobs verbatim — no tar framing, no compression. A manifest
//! that also carries a `subject` descriptor is a *referrer* of that subject:
//! the association the OCI 1.1 referrers API lists.
//!
//! This module is the producer half of OCI-4. The client half —
//! listing referrers via `GET /v2/<name>/referrers/<digest>` plus the
//! `<algo>-<hex>` fallback tag — builds on these manifests (4b).
//!
//! Byte reproducibility holds here exactly as in [`super::emit_image`]: every
//! map is a `BTreeMap`, nothing injects a timestamp, and `layers` follows the
//! caller's slice order, so identical inputs yield an identical manifest
//! digest.

use std::collections::BTreeMap;

use bytes::Bytes;
use oci_client::manifest::{
    ImageIndexEntry, OCI_IMAGE_MEDIA_TYPE, OciDescriptor, OciImageManifest,
};

use crate::registry::ImageLayout;
use crate::registry::error::RegistryError;
use crate::registry::layout::sha256_digest;

/// OCI 1.1 empty-JSON media type — the `config` media type of a bare
/// artifact manifest (image-spec `manifest.md`, "Guidelines for Artifact
/// Usage").
pub const EMPTY_JSON_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";

/// The canonical empty-JSON blob: exactly the two bytes `{}`. Its descriptor
/// is the spec-fixed `size: 2` / well-known sha256 every artifact manifest's
/// `config` points at.
const EMPTY_JSON_BLOB: &[u8] = b"{}";

/// One blob attached to an artifact manifest (one `layers[]` entry).
///
/// The bytes are written to `blobs/sha256/<digest>` as-is — an artifact blob
/// is opaque to UMF and is described solely by its media type, unlike a
/// [`super::LayerSource`], which is always a compressed tar with a `diff_id`.
#[derive(Debug, Clone)]
pub struct ArtifactBlob {
    /// Media type recorded on the blob's descriptor
    /// (e.g. `application/spdx+json`).
    pub media_type: String,
    /// The blob bytes, stored verbatim.
    pub data: Bytes,
    /// Optional descriptor-level annotations. A sorted map, so the emitted
    /// descriptor stays byte-reproducible.
    pub annotations: Option<BTreeMap<String, String>>,
}

/// Convert a layout/index entry (as returned by [`super::emit_image`],
/// [`super::emit_index`], or a registry pull) into the `subject` descriptor
/// that makes an artifact refer to that manifest.
#[must_use]
pub fn subject_from_entry(entry: &ImageIndexEntry) -> OciDescriptor {
    OciDescriptor {
        media_type: entry.media_type.clone(),
        digest: entry.digest.clone(),
        size: entry.size,
        urls: None,
        annotations: None,
    }
}

/// Emit an OCI 1.1 artifact manifest into `layout`.
///
/// Writes the empty-JSON config blob, every `blobs[]` entry, and the manifest
/// itself, then registers the manifest in `index.json`: under `ref_name` when
/// given, as an **untagged** entry otherwise (referrer artifacts are usually
/// digest-addressed). Untagged entries keep the artifact reachable for
/// [`ImageLayout::prune_blobs`] and listable via
/// [`ImageLayout::list_referrers`] without surfacing in the ref table that
/// `umf images` displays.
///
/// `subject` is the manifest this artifact refers to (`None` for a standalone
/// artifact); build it from an emitted image with [`subject_from_entry`].
/// `artifact_type` is required by the spec whenever the config is the empty
/// descriptor, and must be an RFC 6838 `type/subtype` media type — as must
/// every blob's `media_type`.
///
/// A content-less artifact (`blobs` empty) emits the spec-mandated single
/// `layers` entry pointing at the empty descriptor.
///
/// Returns the index entry for the emitted manifest — feed it to
/// [`subject_from_entry`] to chain artifacts, or push it by digest.
pub fn emit_artifact_manifest(
    layout: &ImageLayout,
    artifact_type: &str,
    subject: Option<&OciDescriptor>,
    blobs: &[ArtifactBlob],
    annotations: Option<&BTreeMap<String, String>>,
    ref_name: Option<&str>,
) -> Result<ImageIndexEntry, RegistryError> {
    validate_media_type(artifact_type)?;
    for blob in blobs {
        validate_media_type(&blob.media_type)?;
    }

    // 1. The shared empty-JSON config blob.
    let empty = empty_json_descriptor(layout)?;

    // 2. Artifact blobs. The image-spec requires at least one `layers`
    //    entry, so a content-less artifact points at the empty descriptor.
    let layers = if blobs.is_empty() {
        vec![empty.clone()]
    } else {
        blobs
            .iter()
            .map(|blob| -> Result<OciDescriptor, RegistryError> {
                let digest = sha256_digest(&blob.data);
                layout.write_blob_with_digest(&blob.data, &digest)?;
                Ok(OciDescriptor {
                    media_type: blob.media_type.clone(),
                    digest,
                    size: blob.data.len() as i64,
                    urls: None,
                    annotations: blob.annotations.clone(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    // 3. The manifest — an image manifest with the 1.1 artifact fields set.
    let manifest = OciImageManifest {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_MEDIA_TYPE.to_string()),
        artifact_type: Some(artifact_type.to_string()),
        config: empty,
        layers,
        subject: subject.cloned(),
        annotations: annotations.cloned(),
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = sha256_digest(&manifest_bytes);
    layout.write_blob_with_digest(&manifest_bytes, &manifest_digest)?;

    // 4. Register in index.json — tagged when asked, untagged otherwise —
    //    so the artifact is a prune root and locally listable either way.
    let entry = ImageIndexEntry {
        media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
        digest: manifest_digest,
        size: manifest_bytes.len() as i64,
        platform: None,
        annotations: None,
    };
    match ref_name {
        Some(name) => layout.upsert_ref(name, entry.clone())?,
        None => layout.upsert_untagged(entry.clone())?,
    }
    Ok(entry)
}

/// Write (or re-write, idempotently) the canonical empty-JSON blob and return
/// its descriptor.
fn empty_json_descriptor(layout: &ImageLayout) -> Result<OciDescriptor, RegistryError> {
    let digest = sha256_digest(EMPTY_JSON_BLOB);
    layout.write_blob_with_digest(EMPTY_JSON_BLOB, &digest)?;
    Ok(OciDescriptor {
        media_type: EMPTY_JSON_MEDIA_TYPE.to_string(),
        digest,
        size: EMPTY_JSON_BLOB.len() as i64,
        urls: None,
        annotations: None,
    })
}

/// Lightweight RFC 6838 shape check: `type/subtype`, both halves non-empty,
/// a single `/`, printable ASCII throughout. The image-spec requires
/// `artifactType` (and every descriptor media type) to be a valid media
/// type; the full RFC 6838 grammar is more than the producer needs to
/// enforce, but this catches the realistic mistakes (an empty string, a bare
/// word, embedded whitespace).
fn validate_media_type(media_type: &str) -> Result<(), RegistryError> {
    let shape_ok = media_type
        .split_once('/')
        .is_some_and(|(t, s)| !t.is_empty() && !s.is_empty() && !s.contains('/'));
    if shape_ok && media_type.chars().all(|c| c.is_ascii_graphic()) {
        Ok(())
    } else {
        Err(RegistryError::InvalidMediaType(media_type.to_string()))
    }
}

#[cfg(test)]
mod tests;
