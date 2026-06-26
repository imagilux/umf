//! Read-only inspection of an L0 image already cached in an [`ImageLayout`].
//!
//! Introspection reads the manifest pinned by a ref name, fetches its config
//! blob, and either reads the [`umf_core::label::TYPE`] label or falls back to
//! manifest-shape inference. No network IO — callers are expected to have
//! pulled the L0 first (typically via [`umf_oci::registry::RegistryClient::pull`]).
//!
//! Splitting introspect from pull keeps the function deterministic and
//! air-gapped, and makes it trivial to unit-test against synthetic configs.

use std::collections::BTreeMap;

use oci_client::manifest::{
    IMAGE_CONFIG_MEDIA_TYPE, IMAGE_DOCKER_CONFIG_MEDIA_TYPE, OciImageManifest, OciManifest,
};
use serde::Deserialize;
use umf_core::l0::{L0Kind, L0Source};
use umf_core::label;

use umf_oci::registry::ImageLayout;
use umf_oci::registry::error::RegistryError;

/// Result of introspecting an L0 image.
///
/// Bundles the identified [`L0Kind`] with provenance ([`L0Source`]), the
/// manifest digest that was inspected, and the raw config-Labels map (so
/// downstream callers can look up `org.imagilux.umf.spec` or vendor-specific
/// labels without re-reading the config blob).
#[derive(Debug, Clone)]
pub struct L0Profile {
    /// Identified shape of the L0.
    pub kind: L0Kind,
    /// Whether [`Self::kind`] was read from a label or inferred from manifest
    /// structure.
    pub source: L0Source,
    /// Manifest digest the introspection ran against. Empty for
    /// [`L0Kind::Scratch`].
    pub manifest_digest: String,
    /// Verbatim copy of the OCI config `Labels` map.
    pub labels: BTreeMap<String, String>,
}

impl L0Profile {
    /// Sentinel profile for `FROM scratch`. No image is involved.
    ///
    /// `kind` is [`L0Kind::Scratch`], `source` is [`L0Source::Label`] (the
    /// shape was declared, not inferred), `manifest_digest` is empty, and
    /// `labels` is empty.
    pub fn scratch() -> Self {
        Self {
            kind: L0Kind::Scratch,
            source: L0Source::Label,
            manifest_digest: String::new(),
            labels: BTreeMap::new(),
        }
    }
}

/// Introspect the manifest cached for `ref_name` in `layout`.
///
/// Looks up the ref-name in `index.json`, reads the manifest blob, and reads
/// the config blob it references. If the OCI config carries an
/// [`umf_core::label::TYPE`] label its value is mapped via
/// [`L0Kind::from_label`]; otherwise the kind is inferred from manifest
/// structure (a container-shaped image with at least one layer maps to
/// [`L0Kind::Container`]).
///
/// Returns [`RegistryError::NotFound`] when the ref name is not present in the
/// layout, [`RegistryError::InvalidLayout`] when the manifest is an image
/// index (the caller must select a platform first), and propagates I/O or
/// JSON failures from blob reads.
pub fn introspect(layout: &ImageLayout, ref_name: &str) -> Result<L0Profile, RegistryError> {
    let entry = layout
        .lookup_ref(ref_name)?
        .ok_or_else(|| RegistryError::NotFound(ref_name.to_string()))?;

    let manifest_bytes = layout.read_blob(&entry.digest)?;
    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)?;

    match manifest {
        OciManifest::Image(image) => introspect_image(layout, &entry.digest, &image),
        OciManifest::ImageIndex(_) => Err(RegistryError::InvalidLayout(format!(
            "{ref_name} resolves to an image index — select a platform before introspection",
        ))),
    }
}

fn introspect_image(
    layout: &ImageLayout,
    manifest_digest: &str,
    manifest: &OciImageManifest,
) -> Result<L0Profile, RegistryError> {
    let config_bytes = layout.read_blob(&manifest.config.digest)?;
    let doc: ImageConfigDoc = serde_json::from_slice(&config_bytes)?;
    let labels = doc.config.labels;

    let (kind, source) = match labels.get(label::TYPE) {
        Some(value) => (L0Kind::from_label(value), L0Source::Label),
        None => (infer_kind(manifest), L0Source::Inferred),
    };

    Ok(L0Profile {
        kind,
        source,
        manifest_digest: manifest_digest.to_string(),
        labels,
    })
}

/// Best-effort manifest-shape inference when `org.imagilux.umf.type` is absent.
///
/// "Container-shaped" — an OCI image config (either the spec's
/// `application/vnd.oci.image.config.v1+json` or the legacy
/// `application/vnd.docker.container.image.v1+json` that older
/// registries still emit) plus at least one layer — maps to
/// [`L0Kind::Container`]. Anything else returns `L0Kind::Unknown("")`
/// rather than guessing at a more specific shape: the spec only
/// commits to container-shape inference, so we don't manufacture
/// stronger claims.
fn infer_kind(manifest: &OciImageManifest) -> L0Kind {
    let config_mt = manifest.config.media_type.as_str();
    let container_like = matches!(
        config_mt,
        IMAGE_CONFIG_MEDIA_TYPE | IMAGE_DOCKER_CONFIG_MEDIA_TYPE,
    );
    if container_like && !manifest.layers.is_empty() {
        L0Kind::Container
    } else {
        L0Kind::Unknown(String::new())
    }
}

// ── Minimal OCI image config shape used for label extraction ─────────────────
//
// The full OCI image config (`application/vnd.oci.image.config.v1+json`) has
// many fields (env, cmd, entrypoint, rootfs, history, …). For introspection
// we only need `config.Labels`; everything else is allowed-but-ignored.

#[derive(Debug, Default, Deserialize)]
struct ImageConfigDoc {
    #[serde(default)]
    config: ConfigSection,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigSection {
    #[serde(default, rename = "Labels")]
    labels: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests;
