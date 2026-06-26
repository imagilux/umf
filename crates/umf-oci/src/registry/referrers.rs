//! OCI 1.1 referrers — the fallback tag schema and the wire types a
//! referrers listing travels as (OCI-4b, the client half).
//!
//! A *referrer* is a manifest whose `subject` descriptor points at another
//! manifest (see [`crate::image::emit_artifact_manifest`]). Registries that
//! implement the OCI 1.1 referrers API list them at
//! `GET /v2/<name>/referrers/<digest>`; for registries that don't, the
//! distribution spec defines a **fallback tag schema**: the *client*
//! maintains an image index under the tag [`fallback_tag`]`(<digest>)` whose
//! descriptors carry each referrer's `artifactType` and annotations.
//!
//! The index document is hand-rolled here rather than reusing
//! `oci_client::manifest::OciImageIndex` — same policy as
//! `crate::image::serde` — because `oci-client` 0.16's `ImageIndexEntry`
//! cannot carry the per-descriptor `artifactType` the referrers responses
//! require. `BTreeMap` annotations plus digest-sorted descriptors keep the
//! maintained fallback index deterministic for a given referrer set.

use std::collections::BTreeMap;

use oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE;
use serde::{Deserialize, Serialize};

use super::error::RegistryError;

/// OCI distribution-spec tag grammar caps a tag at 128 characters; the
/// referrers tag schema says to truncate the `<algo>-<hex>` form to fit.
const MAX_TAG_LEN: usize = 127;

/// One referrer in a referrers listing — an image-spec descriptor plus the
/// `artifactType` / `annotations` fields the distribution spec requires
/// referrers responses to carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferrerDescriptor {
    /// Media type of the referrer manifest (normally
    /// `application/vnd.oci.image.manifest.v1+json`).
    pub media_type: String,
    /// Digest of the referrer manifest.
    pub digest: String,
    /// Size in bytes of the referrer manifest.
    pub size: i64,
    /// The referrer's `artifactType` — what the artifact *is*.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    /// The referrer manifest's annotations, copied verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

/// The image-index document a referrers listing travels as: the response
/// body of `GET /v2/<name>/referrers/<digest>` and the manifest content of
/// the fallback tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferrersIndex {
    /// Always `2`.
    pub schema_version: u8,
    /// Always [`OCI_IMAGE_INDEX_MEDIA_TYPE`].
    pub media_type: String,
    /// The referrer descriptors.
    pub manifests: Vec<ReferrerDescriptor>,
}

impl ReferrersIndex {
    /// An index with no referrers — what a missing fallback tag denotes.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: 2,
            media_type: OCI_IMAGE_INDEX_MEDIA_TYPE.to_string(),
            manifests: Vec::new(),
        }
    }

    /// Insert — or replace, keyed by digest — a referrer descriptor, keeping
    /// the list digest-sorted so the fallback tag's read-modify-write is
    /// deterministic for a given referrer set.
    pub fn upsert(&mut self, descriptor: ReferrerDescriptor) {
        self.manifests.retain(|d| d.digest != descriptor.digest);
        self.manifests.push(descriptor);
        self.manifests.sort_by(|a, b| a.digest.cmp(&b.digest));
    }

    /// The descriptors, dropped to those whose `artifactType` equals
    /// `filter` — the client-side filtering the spec prescribes for the
    /// fallback path (the API path filters server-side instead).
    #[must_use]
    pub fn filtered(self, filter: Option<&str>) -> Vec<ReferrerDescriptor> {
        match filter {
            None => self.manifests,
            Some(at) => self
                .manifests
                .into_iter()
                .filter(|d| d.artifact_type.as_deref() == Some(at))
                .collect(),
        }
    }
}

/// The fallback referrers tag for a subject digest (distribution-spec,
/// "Referrers Tag Schema"): `<algo>-<hex>` — `sha256:abc…` → `sha256-abc…`,
/// truncated to the 127-character tag limit (relevant for sha512).
///
/// # Errors
/// [`RegistryError::MalformedDigest`] when `digest` is not `algo:hex`.
pub fn fallback_tag(digest: &str) -> Result<String, RegistryError> {
    let well_formed = digest.split_once(':').is_some_and(|(algo, hex)| {
        !algo.is_empty()
            && !hex.is_empty()
            && algo
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && hex.chars().all(|c| c.is_ascii_hexdigit())
    });
    if !well_formed {
        return Err(RegistryError::MalformedDigest(digest.to_string()));
    }
    let mut tag = digest.replace(':', "-");
    tag.truncate(MAX_TAG_LEN);
    Ok(tag)
}

#[cfg(test)]
mod tests;
