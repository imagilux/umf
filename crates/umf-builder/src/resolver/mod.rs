//! Component resolution: `registry → local cache → source build`.
//!
//! Applies uniformly to every OCI reference (FROM, `ADD --from=<image>`).
//! Sovereignty-first: an air-gapped node must be able to produce any artifact
//! from source alone — registries and caches are accelerators, never
//! preconditions.
//!
//! This module exposes:
//!
//! * [`resolve_add`] — picks the OCI image an `ADD --from=<image>` references
//!   (the base userland / rootfs for a bootable build). The ref shape is the
//!   ordinary OCI reference form (`alpine:3.21.0`, `debian:bookworm`,
//!   `myorg/curated-rootfs:1.0`); resolution flows through the standard OCI
//!   distribution pipeline, not any per-distro special case.
//! * [`resolve_from_kernel`] — picks the kernel artifact a bootable build's
//!   `FROM` references. Same OCI pull pipeline as the add resolver, plus an
//!   `org.imagilux.umf.type=kernel` label check on the resolved artifact.
//!
//! Initramfs assembly happens in [`crate::initrd`], not here — it operates
//! on the staged kernel binaries rather than on a network-resolved artifact.
//! The bootloader is no longer resolved here either: `umf compile` installs it
//! at projection time per the image's flavor label (preferring a bootloader
//! shipped inside the rootfs, falling back to the host).
//!
//! `resolve_add` and `resolve_from_kernel` share the layer-artifact resolution
//! ladder (`override → cache → registry`) through the private `LayerResolve`
//! trait + `resolve_layers` core below; they differ only in their
//! error/artifact types and in the post-pull introspection step (FROM-kernel
//! verifies an `org.imagilux.umf.type=kernel` label; the add resolver does not).

mod add;
mod from_kernel;

pub use add::{AddArtifact, AddProvenance, AddResolveError, resolve_add};
pub use from_kernel::{
    FromKernelArtifact, FromKernelProvenance, FromKernelResolveError, resolve_from_kernel,
};

use std::path::{Path, PathBuf};

use oci_client::Reference;
use oci_client::manifest::{OciImageManifest, OciManifest};
use tracing::{debug, warn};
use umf_core::architecture::Architecture;
use umf_oci::image::select_manifest_for_arch;
use umf_oci::registry::auth::{CredentialOverride, resolve_auth_for};
use umf_oci::registry::error::RegistryError;
use umf_oci::registry::{ImageLayout, RegistryClient, SearchRegistries, resolution_candidates};

/// Where a layer-based artifact was resolved from. Mapped onto each
/// resolver's own public provenance enum by [`LayerResolve::artifact`].
#[derive(Debug, Clone)]
enum Provenance {
    /// Caller-supplied override path (single layer).
    Override(PathBuf),
    /// Pulled fresh from a remote registry into the layout cache.
    Registry(String),
    /// Already present in the on-disk image-layout cache; no network.
    Cache(String),
}

/// The per-resolver bits the shared [`resolve_layers`] ladder needs: the
/// concrete artifact/error types, the human label used in log lines and the
/// override-missing message, the typed error constructors, and the optional
/// post-pull introspection check (FROM-kernel verifies a label; rootfs does
/// not).
///
/// Keeping this as a trait — rather than threading half a dozen closures
/// through `resolve_layers` — lets each resolver stay a thin marker while the
/// ladder logic lives in exactly one place.
trait LayerResolve {
    /// The resolver's public artifact type (carries `layers` + provenance).
    type Artifact;
    /// The resolver's public error type. The blanket bounds let `?` carry
    /// `RegistryError` / `serde_json` / `io` failures through unchanged; the
    /// `Debug` bound lets the ladder log a failed cache/registry attempt
    /// before falling through to the next rung.
    type Error: std::fmt::Debug
        + From<RegistryError>
        + From<serde_json::Error>
        + From<std::io::Error>;

    /// Human label for log lines / override-missing message
    /// (e.g. `"rootfs"`, `"FROM kernel"`).
    const LABEL: &'static str;

    /// Build the public artifact from resolved layer paths + provenance.
    fn artifact(layers: Vec<PathBuf>, provenance: Provenance) -> Self::Artifact;

    /// Construct the "malformed OCI reference" error.
    fn malformed_ref(ref_name: String, detail: String) -> Self::Error;

    /// Construct the "malformed artifact" error.
    fn malformed_artifact(ref_name: String, detail: String) -> Self::Error;

    /// Construct the "chain exhausted" error.
    fn not_found(tried: String) -> Self::Error;

    /// Optional introspection gate run before reading layers out of the
    /// cache. Default is a no-op (rootfs); FROM-kernel overrides it to
    /// reject anything not labelled `org.imagilux.umf.type=kernel`.
    fn post_pull_check(_layout: &ImageLayout, _ref_name: &str) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Read the layer blob paths for an already-cached artifact: run the
/// resolver's post-pull check, then read the (arch-selected) manifest, reject
/// empty layer sets, and map each layer digest to its on-disk blob path. The
/// caller stamps the appropriate [`Provenance`] (cache vs registry) onto the
/// result.
fn read_layer_paths<R: LayerResolve>(
    layout: &ImageLayout,
    ref_name: &str,
    architecture: Architecture,
) -> Result<Vec<PathBuf>, R::Error> {
    R::post_pull_check(layout, ref_name)?;

    let entry = layout.lookup_ref(ref_name)?.ok_or_else(|| {
        R::malformed_artifact(
            ref_name.into(),
            "ref vanished from layout during read".into(),
        )
    })?;
    let manifest = read_image_manifest::<R>(layout, ref_name, &entry.digest, architecture)?;
    if manifest.layers.is_empty() {
        return Err(R::malformed_artifact(
            ref_name.into(),
            "no layers in manifest".into(),
        ));
    }
    manifest
        .layers
        .iter()
        .map(|l| layout.blob_path(&l.digest))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Read the OCI image manifest stored at `digest`, transparently descending a
/// multi-arch image **index** to the child whose platform matches
/// `architecture`. A bare `FROM` / `ADD --from=<image>` of a stock multi-arch
/// base (e.g. `alpine:3.21`) resolves to an index whose top manifest has no
/// `config` field; selecting the platform child here mirrors the container
/// base-image path (`engine_build::base_image`) and `umf inspect --platform`,
/// all of which share [`select_manifest_for_arch`].
fn read_image_manifest<R: LayerResolve>(
    layout: &ImageLayout,
    ref_name: &str,
    digest: &str,
    architecture: Architecture,
) -> Result<OciImageManifest, R::Error> {
    let bytes = layout.read_blob(digest)?;
    match serde_json::from_slice::<OciManifest>(&bytes)? {
        OciManifest::Image(image) => Ok(image),
        OciManifest::ImageIndex(index) => {
            let arch = architecture.oci_arch_string();
            let child = select_manifest_for_arch(&index, arch).ok_or_else(|| {
                R::malformed_artifact(
                    ref_name.into(),
                    format!("image index has no linux/{arch} manifest"),
                )
            })?;
            let child_bytes = layout.read_blob(&child.digest)?;
            Ok(serde_json::from_slice(&child_bytes)?)
        }
    }
}

/// Pull from the registry into the cache, then read the layers back out and
/// stamp the provenance as [`Provenance::Registry`].
async fn pull_from_registry<R: LayerResolve>(
    client: &RegistryClient,
    layout: &ImageLayout,
    reference: &Reference,
    architecture: Architecture,
) -> Result<R::Artifact, R::Error> {
    let auth = resolve_auth_for(Some(reference.registry()), &CredentialOverride::default());
    client.pull(reference, &auth, layout).await?;
    let canonical = reference.whole();
    let layers = read_layer_paths::<R>(layout, &canonical, architecture)?;
    Ok(R::artifact(layers, Provenance::Registry(canonical)))
}

/// Resolve a layer-based OCI artifact (rootfs or FROM-kernel) through the
/// shared `override → cache → registry` ladder.
///
/// 1. `override_path` — when supplied, returns immediately with a
///    single-layer artifact (errors if the path isn't a file).
/// 2. Cache — `registry_ref` (or `reference` directly) looked up in
///    `layout`; no network.
/// 3. Registry pull — only when a `registry` client is supplied.
///
/// Source-build fallback (the sovereignty endpoint) is not yet wired for
/// either resolver.
async fn resolve_layers<R: LayerResolve>(
    reference: &str,
    registry: Option<&RegistryClient>,
    layout: &ImageLayout,
    override_path: Option<&Path>,
    registry_ref: Option<&str>,
    architecture: Architecture,
) -> Result<R::Artifact, R::Error> {
    if let Some(path) = override_path {
        debug!(label = R::LABEL, path = %path.display(), "using explicit override");
        if !path.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} override path {} not found", R::LABEL, path.display()),
            )
            .into());
        }
        return Ok(R::artifact(
            vec![path.to_path_buf()],
            Provenance::Override(path.to_path_buf()),
        ));
    }

    let pull_ref = registry_ref.unwrap_or(reference);
    // Validate up front so a genuinely malformed ref is a hard error rather
    // than a chain-exhausted miss (preserves the prior behaviour).
    let _: Reference = pull_ref
        .parse()
        .map_err(|e: oci_client::ParseError| R::malformed_ref(pull_ref.into(), e.to_string()))?;

    // Expand an *unqualified* ref against the operator's search registries (then
    // the docker.io default). A qualified ref, or an empty search list, yields
    // just itself, so resolution is unchanged unless registries are configured
    // via `umf registry`.
    let search = SearchRegistries::load().search;
    let candidates = resolution_candidates(pull_ref, &search);

    let mut tried: Vec<String> = Vec::new();
    for candidate in &candidates {
        let Ok(oci_ref) = candidate.parse::<Reference>() else {
            continue;
        };
        let canonical = oci_ref.whole();

        // 1. Cache — check the on-disk image-layout first; no network.
        tried.push(format!("cache {canonical}"));
        if layout.lookup_ref(&canonical)?.is_some() {
            match read_layer_paths::<R>(layout, &canonical, architecture) {
                Ok(layers) => {
                    debug!(label = R::LABEL, ref_name = %canonical, "cache hit");
                    return Ok(R::artifact(layers, Provenance::Cache(canonical)));
                }
                Err(err) => warn!(label = R::LABEL, ?err, "cache read failed, continuing"),
            }
        }

        // 2. Registry pull — if a client is supplied.
        if let Some(client) = registry {
            tried.push(format!("registry {canonical}"));
            match pull_from_registry::<R>(client, layout, &oci_ref, architecture).await {
                Ok(art) => {
                    debug!(label = R::LABEL, ref_name = %canonical, "registry pull succeeded");
                    return Ok(art);
                }
                Err(err) => warn!(label = R::LABEL, ?err, "registry pull failed, continuing"),
            }
        } else {
            tried.push(format!("registry {canonical} (skipped — no client)"));
        }
    }

    Err(R::not_found(tried.join(", ")))
}
