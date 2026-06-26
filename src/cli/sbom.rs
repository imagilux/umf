//! `umf sbom` — attach SBOM documents to an image as OCI 1.1 referrer
//! artifacts.
//!
//! `umf sbom attach <image> --sbom <file>` writes a bare artifact manifest
//! whose `subject` is the image's manifest and whose single blob is the SBOM
//! document verbatim (SPDX or CycloneDX JSON), with the document's media type
//! as the `artifactType`. This is the cosign/oras-compatible attachment
//! shape: a referrers-aware client (`oras discover`, `cosign tree`) lists it
//! against the image. `--push` uploads the referrer to the image's registry,
//! maintaining the OCI 1.1 referrers index (or its `<algo>-<hex>` fallback
//! tag).
//!
//! The subject image must already be in the local layout; `--push` further
//! assumes it is already in that registry (push the image, then its SBOM).

use std::collections::BTreeMap;
use std::path::Path;

use oci_client::Reference;
use oci_client::manifest::ImageIndexEntry;
use thiserror::Error;
use tracing::info;
use umf_oci::image::{ArtifactBlob, emit_artifact_manifest, subject_from_entry};
use umf_oci::registry::{ImageLayout, RegistryError};

use crate::cli::SbomFormat;
use crate::cli::util::{self, CredentialError};

/// SPDX JSON artifact / blob media type.
const SPDX_MEDIA_TYPE: &str = "application/spdx+json";
/// CycloneDX JSON artifact / blob media type.
const CYCLONEDX_MEDIA_TYPE: &str = "application/vnd.cyclonedx+json";

#[derive(Debug, Error)]
pub(crate) enum CliSbomError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
    #[error("invalid OCI reference {reference:?}: {err}")]
    BadReference {
        reference: String,
        err: oci_client::ParseError,
    },
    #[error("image not found in layout: {0} (build or pull it first, e.g. `umf pull {0}`)")]
    ImageNotFound(String),
    #[error("SBOM file {path}: {err}")]
    SbomRead { path: String, err: std::io::Error },
    #[error(
        "could not detect the SBOM format of {0}; pass --format spdx|cyclonedx (looked for an \
        SPDX `spdxVersion` or a CycloneDX `bomFormat` field)"
    )]
    UndetectableFormat(String),
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest / config JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("materialize image rootfs: {0}")]
    Materialize(#[from] umf_oci::materialize::MaterializeError),
    #[error(
        "{0} is a multi-arch image index, not a single image; pass a per-arch reference to scan a \
        concrete rootfs"
    )]
    IndexNotScannable(String),
}

impl From<CredentialError> for CliSbomError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf sbom attach` flags.
pub(crate) struct AttachArgs<'a> {
    /// Image whose manifest the SBOM refers to (the `subject`).
    pub(crate) reference: &'a str,
    /// Path to the SBOM document.
    pub(crate) sbom: &'a Path,
    /// Forced SBOM format; auto-detected from the document when `None`.
    pub(crate) format: Option<SbomFormat>,
    /// Push the referrer to the registry implied by `reference`.
    pub(crate) push: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
}

/// Attach an SBOM document to `reference` as a referrer artifact, writing it
/// into the layout and optionally pushing it.
pub(crate) fn run_attach(args: AttachArgs<'_>) -> Result<(), CliSbomError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliSbomError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;

    // The SBOM refers to this image's manifest — resolve it to a subject.
    let subject_entry = layout
        .lookup_ref(args.reference)?
        .ok_or_else(|| CliSbomError::ImageNotFound(args.reference.to_string()))?;
    let subject = subject_from_entry(&subject_entry);

    let data = std::fs::read(args.sbom).map_err(|err| CliSbomError::SbomRead {
        path: args.sbom.display().to_string(),
        err,
    })?;
    let media_type = match args.format {
        Some(SbomFormat::Spdx) => SPDX_MEDIA_TYPE,
        Some(SbomFormat::Cyclonedx) => CYCLONEDX_MEDIA_TYPE,
        None => detect_format(&data)
            .ok_or_else(|| CliSbomError::UndetectableFormat(args.sbom.display().to_string()))?,
    };

    // A title annotation lets referrers-aware tooling show the document name.
    // Derived from the filename, so emission stays byte-reproducible (no
    // timestamps, no host paths leak into the descriptor).
    let mut blob_annotations = BTreeMap::new();
    if let Some(name) = args.sbom.file_name().and_then(|n| n.to_str()) {
        blob_annotations.insert(
            "org.opencontainers.image.title".to_string(),
            name.to_string(),
        );
    }
    let blob = ArtifactBlob {
        media_type: media_type.to_string(),
        data: bytes::Bytes::from(data),
        annotations: (!blob_annotations.is_empty()).then_some(blob_annotations),
    };

    let entry = emit_artifact_manifest(
        &layout,
        media_type,
        Some(&subject),
        std::slice::from_ref(&blob),
        None,
        None,
    )?;
    info!(
        subject = %subject.digest,
        artifact = %entry.digest,
        media_type = %media_type,
        "attached SBOM referrer",
    );
    println!(
        "Attached {media_type} SBOM {artifact} -> {subject}",
        artifact = entry.digest,
        subject = subject.digest,
    );

    if args.push {
        push_referrer(
            &layout,
            args.reference,
            args.username,
            args.password_stdin,
            args.insecure_registry,
            &entry,
        )?;
    }
    Ok(())
}

/// Sniff SPDX vs CycloneDX from the document body. SPDX JSON carries a
/// top-level `spdxVersion`; CycloneDX carries `bomFormat: "CycloneDX"`. A
/// substring check is enough to pick a media type — the bytes are stored
/// verbatim regardless, so a full parse would buy nothing.
fn detect_format(data: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(data).ok()?;
    if text.contains("\"spdxVersion\"") {
        Some(SPDX_MEDIA_TYPE)
    } else if text.contains("\"bomFormat\"") && text.contains("\"CycloneDX\"") {
        Some(CYCLONEDX_MEDIA_TYPE)
    } else {
        None
    }
}

/// Push the just-attached referrer to the registry implied by `reference`.
/// The subject image must already be present in that registry for the
/// referrers association to resolve.
fn push_referrer(
    layout: &ImageLayout,
    reference_str: &str,
    username: Option<&str>,
    password_stdin: bool,
    insecure_registry: bool,
    entry: &ImageIndexEntry,
) -> Result<(), CliSbomError> {
    let reference: Reference = reference_str
        .parse()
        .map_err(|err: oci_client::ParseError| CliSbomError::BadReference {
            reference: reference_str.to_string(),
            err,
        })?;
    util::push_referrer_for(
        layout,
        &reference,
        username,
        password_stdin,
        insecure_registry,
        entry,
    )
}

mod generate;
mod scan;

pub(crate) use generate::{GenerateArgs, run_generate};

#[cfg(test)]
mod tests;
