//! FROM kernel resolver.
//!
//! In bootable builds, `FROM <ref>` references the kernel
//! artifact. Resolution mirrors [`crate::resolver::resolve_rootfs`] — same
//! OCI pull pipeline, same on-disk layout cache. The difference is a label
//! introspection step that rejects anything not marked
//! `org.imagilux.umf.type=kernel`.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;
use umf_core::architecture::Architecture;
use umf_core::l0::{L0Kind, Payload};

use crate::introspect::introspect;
use umf_oci::registry::error::RegistryError;
use umf_oci::registry::{ImageLayout, RegistryClient};

use super::{LayerResolve, Provenance, resolve_layers};

/// Errors produced by the FROM kernel resolver.
#[derive(Debug, Error)]
pub enum FromKernelResolveError {
    /// The chain ran to the end without finding a usable kernel artifact.
    #[error("no kernel artifact found (tried {tried})")]
    NotFound {
        /// Comma-separated list of locations that were probed, in order.
        tried: String,
    },

    /// The reference didn't parse as a valid OCI reference.
    #[error("malformed OCI reference {ref_name:?}: {detail}")]
    MalformedRef {
        /// The offending input.
        ref_name: String,
        /// Underlying parse failure.
        detail: String,
    },

    /// The pulled artifact didn't carry the expected
    /// `org.imagilux.umf.type=kernel` label, or its manifest was empty.
    #[error("FROM artifact {ref_name:?} is not a kernel: {detail}")]
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

/// Resolved kernel artifact pulled via a VM build's `FROM` reference. The
/// `layers` paths point at the on-disk tarballs that, when unpacked in order
/// into a staging tree, expose `boot/vmlinuz-<release>` and
/// `lib/modules/<release>/`.
#[derive(Debug)]
pub struct FromKernelArtifact {
    /// Filesystem path(s) to the kernel artifact's layers, in extraction
    /// order. Always non-empty when the resolver returns `Ok`.
    pub layers: Vec<PathBuf>,
    /// Where the resolver found the kernel artifact.
    pub provenance: FromKernelProvenance,
    // Kept private so callers can't split the layer list from any backing
    // scratch storage. `None` for cache / override paths.
    _scratch: Option<TempDir>,
}

/// Provenance of a resolved FROM kernel artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromKernelProvenance {
    /// The caller passed an explicit override path (test seam — there is no
    /// CLI flag exposing this).
    Override(PathBuf),
    /// Pulled fresh from a remote OCI registry into the layout cache.
    Registry(String),
    /// Already cached in the on-disk OCI image-layout — no network access.
    Cache(String),
}

/// Resolve a VM build's `FROM <ref>` as a kernel artifact. Resolution chain:
///
/// 1. `override_path` — when supplied, returns immediately with a single-layer
///    artifact (test/sovereign seam; no CLI flag is wired through to this).
/// 2. Cache — `registry_ref` (or `reference` directly) looked up in `layout`;
///    no network.
/// 3. Registry pull — only when a `registry` client is supplied.
///
/// Upstream source build (compile from kernel.org tarball) is the spec's
/// sovereignty endpoint but isn't wired here — the kernel-from-FROM path
/// expects an OCI artifact produced by a separate kernel-build pipeline.
pub async fn resolve_from_kernel(
    reference: &str,
    architecture: Architecture,
    registry: Option<&RegistryClient>,
    layout: &ImageLayout,
    override_path: Option<&Path>,
    registry_ref: Option<&str>,
) -> Result<FromKernelArtifact, FromKernelResolveError> {
    resolve_layers::<FromKernelResolver>(
        reference,
        registry,
        layout,
        override_path,
        registry_ref,
        architecture,
    )
    .await
}

/// Marker wiring the FROM-kernel artifact/error/provenance types into the
/// shared [`resolve_layers`] ladder. Its [`LayerResolve::post_pull_check`]
/// rejects any cached artifact not labelled `org.imagilux.umf.type=kernel`,
/// which is the one place this resolver diverges from rootfs.
struct FromKernelResolver;

impl LayerResolve for FromKernelResolver {
    type Artifact = FromKernelArtifact;
    type Error = FromKernelResolveError;
    const LABEL: &'static str = "FROM kernel";

    fn artifact(layers: Vec<PathBuf>, provenance: Provenance) -> Self::Artifact {
        FromKernelArtifact {
            layers,
            provenance: match provenance {
                Provenance::Override(p) => FromKernelProvenance::Override(p),
                Provenance::Registry(r) => FromKernelProvenance::Registry(r),
                Provenance::Cache(r) => FromKernelProvenance::Cache(r),
            },
            _scratch: None,
        }
    }

    fn malformed_ref(ref_name: String, detail: String) -> Self::Error {
        FromKernelResolveError::MalformedRef { ref_name, detail }
    }

    fn malformed_artifact(ref_name: String, detail: String) -> Self::Error {
        FromKernelResolveError::MalformedArtifact { ref_name, detail }
    }

    fn not_found(tried: String) -> Self::Error {
        FromKernelResolveError::NotFound { tried }
    }

    fn post_pull_check(layout: &ImageLayout, ref_name: &str) -> Result<(), Self::Error> {
        let profile = introspect(layout, ref_name)?;
        if profile.kind != L0Kind::Payload(Payload::Kernel) {
            return Err(FromKernelResolveError::MalformedArtifact {
                ref_name: ref_name.into(),
                detail: format!(
                    "expected org.imagilux.umf.type=kernel, got {:?}",
                    profile.kind
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
