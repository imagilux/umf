//! `ADD --from=<image>` resolver.
//!
//! `ADD --from=<image-ref> <src> <dst>` pulls an external OCI image and unpacks
//! its layers as build content — the same kind of artifact any `FROM <ref>`
//! line pulls. In a bootable build this is how the base userland (rootfs) is
//! supplied, replacing the former `ROOTFS` directive: the distros' own official
//! images on the standard registries *are* the rootfs sources. Resolution flows
//! through the standard OCI distribution pipeline (oci-client + the on-disk
//! image-layout cache) for every image equally — `alpine:3.21.0`,
//! `debian:bookworm`, `myorg/curated-rootfs:1.0` all share one code path.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;
use umf_core::architecture::Architecture;

use umf_oci::registry::error::RegistryError;
use umf_oci::registry::{ImageLayout, RegistryClient};

use super::{LayerResolve, Provenance, resolve_layers};

/// Errors produced by the `ADD --from=<image>` resolver.
#[derive(Debug, Error)]
pub enum AddResolveError {
    /// The chain ran to the end without resolving the image.
    #[error("ADD --from image not found (tried {tried})")]
    NotFound {
        /// Comma-separated list of locations that were probed, in order.
        tried: String,
    },

    /// The reference didn't parse as a valid OCI reference (the standard OCI
    /// distribution shape: `[registry/]name[:tag|@digest]`).
    #[error("malformed OCI reference {ref_name:?}: {detail}")]
    MalformedRef {
        /// The offending input.
        ref_name: String,
        /// Underlying parse failure.
        detail: String,
    },

    /// A pulled image's manifest doesn't carry any layers — nothing to extract.
    #[error("ADD --from image {ref_name:?} is malformed: {detail}")]
    MalformedArtifact {
        /// The artifact's ref name.
        ref_name: String,
        /// What went wrong.
        detail: String,
    },

    /// Registry / layout error.
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),

    /// JSON decode of an OCI manifest.
    #[error("OCI manifest: {0}")]
    Json(#[from] serde_json::Error),

    /// Filesystem error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolved `ADD --from=<image>` content: the OCI image layers (in extraction
/// order) to unpack into staging, plus provenance.
///
/// Multi-layer images are unpacked in manifest order. The single-layer case
/// (most public distro base images) is the common path; OCI `.wh.foo` whiteouts
/// aren't currently honoured by the staging unpack — fine for the squashed-layer
/// publishing pattern, worth knowing for stacked custom images.
#[derive(Debug)]
pub struct AddArtifact {
    /// Filesystem path(s) to the layer tarball(s), in extraction order.
    /// Always non-empty when the resolver returns `Ok`.
    pub layers: Vec<PathBuf>,
    /// Where the resolver found the image. Useful for logging and the build log.
    pub provenance: AddProvenance,
    // Kept private so callers can't accidentally split the path list from any
    // backing scratch storage. None for cache / override paths.
    _scratch: Option<TempDir>,
}

/// Provenance of a resolved `ADD --from=<image>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddProvenance {
    /// The caller passed an explicit override path. One layer.
    Override(PathBuf),
    /// Pulled fresh from a remote OCI registry into the layout cache.
    Registry(String),
    /// Already cached in the on-disk OCI image-layout — no network access.
    Cache(String),
}

/// Resolve an `ADD --from=<image>` reference. `<image>` is an OCI reference of
/// the same shape `FROM` consumes — `alpine:3.21.0`, `debian:bookworm`,
/// `myorg/curated-rootfs:1.0`, `registry.example.com/mirror/alpine:3.21.0@sha256:...`.
/// Resolution:
///
/// 1. `override_path` — when supplied, returns immediately with a single-layer
///    artifact (test seam).
/// 2. `registry_ref` — explicit override for the OCI ref to pull. Defaults to
///    `reference`.
/// 3. Cache — the OCI ref looked up directly in the layout; no network.
/// 4. Registry pull — only when a `registry` client is also supplied.
///
/// Source-build fallback (for sovereign builds without registry access) is not
/// yet wired — the spec's `registry → cache → source build` chain is two-thirds
/// done here.
pub async fn resolve_add(
    reference: &str,
    architecture: Architecture,
    registry: Option<&RegistryClient>,
    layout: &ImageLayout,
    override_path: Option<&Path>,
    registry_ref: Option<&str>,
) -> Result<AddArtifact, AddResolveError> {
    resolve_layers::<AddResolver>(
        reference,
        registry,
        layout,
        override_path,
        registry_ref,
        architecture,
    )
    .await
}

/// Marker wiring the add artifact/error/provenance types into the shared
/// [`resolve_layers`] ladder. No post-pull introspection — any OCI image with
/// at least one layer is valid content — so it leaves
/// [`LayerResolve::post_pull_check`] at its no-op default.
struct AddResolver;

impl LayerResolve for AddResolver {
    type Artifact = AddArtifact;
    type Error = AddResolveError;
    const LABEL: &'static str = "ADD --from image";

    fn artifact(layers: Vec<PathBuf>, provenance: Provenance) -> Self::Artifact {
        AddArtifact {
            layers,
            provenance: match provenance {
                Provenance::Override(p) => AddProvenance::Override(p),
                Provenance::Registry(r) => AddProvenance::Registry(r),
                Provenance::Cache(r) => AddProvenance::Cache(r),
            },
            _scratch: None,
        }
    }

    fn malformed_ref(ref_name: String, detail: String) -> Self::Error {
        AddResolveError::MalformedRef { ref_name, detail }
    }

    fn malformed_artifact(ref_name: String, detail: String) -> Self::Error {
        AddResolveError::MalformedArtifact { ref_name, detail }
    }

    fn not_found(tried: String) -> Self::Error {
        AddResolveError::NotFound { tried }
    }
}

#[cfg(test)]
mod tests;
