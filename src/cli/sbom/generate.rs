//! `umf sbom generate` — scan an image's installed packages and emit an SBOM.
//!
//! Resolves the image to its layer set, materializes the merged rootfs into a
//! tempdir (whiteout-correct, via `umf_oci::materialize`), reads whichever
//! package database is present ([`super::scan`]), and serializes the result to
//! a deterministic SPDX 2.3 or CycloneDX 1.5 document. The document can be
//! written to a file / stdout and/or attached to the image as a referrer (the
//! same artifact shape as `umf sbom attach`).

use std::io::Write as _;
use std::path::Path;

use oci_client::manifest::OciManifest;
use serde_json::{Value, json};
use tempfile::TempDir;
use tracing::info;
use umf_oci::image::{ArtifactBlob, emit_artifact_manifest, subject_from_entry};
use umf_oci::materialize::materialize_layers;
use umf_oci::registry::ImageLayout;

use super::scan::{Package, scan_rootfs};
use super::{CYCLONEDX_MEDIA_TYPE, CliSbomError, SPDX_MEDIA_TYPE, push_referrer};
use crate::cli::SbomFormat;
use crate::cli::util;

/// Bundled `umf sbom generate` flags.
pub(crate) struct GenerateArgs<'a> {
    /// Image to scan (must be present in the local layout).
    pub(crate) reference: &'a str,
    /// Output document format (default SPDX).
    pub(crate) format: SbomFormat,
    /// Write the document here (`-` is stdout). Independent of `--attach`.
    pub(crate) output: Option<&'a Path>,
    /// Attach the generated document to the image as a referrer artifact.
    pub(crate) attach: bool,
    /// Push the attached referrer (the subject image must already be pushed).
    pub(crate) push: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
}

/// Scan `reference`'s rootfs and emit an SBOM per [`GenerateArgs`].
pub(crate) fn run_generate(args: GenerateArgs<'_>) -> Result<(), CliSbomError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliSbomError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;

    let entry = layout
        .lookup_ref(args.reference)?
        .ok_or_else(|| CliSbomError::ImageNotFound(args.reference.to_string()))?;

    // Resolve the image to its layer set + recorded build time.
    let manifest_bytes = layout.read_blob(&entry.digest)?;
    let image = match serde_json::from_slice::<OciManifest>(&manifest_bytes)? {
        OciManifest::Image(img) => img,
        OciManifest::ImageIndex(_) => {
            return Err(CliSbomError::IndexNotScannable(args.reference.to_string()));
        }
    };
    let created = image_created(&layout, &image.config.digest);
    let layer_digests: Vec<String> = image.layers.iter().map(|l| l.digest.clone()).collect();

    // Materialize the merged rootfs and read its package database.
    let root = TempDir::new()?;
    materialize_layers(&layout, &layer_digests, root.path())?;
    let inventory = scan_rootfs(root.path())?;
    info!(
        packages = inventory.packages.len(),
        os = inventory.os_id.as_deref().unwrap_or("unknown"),
        "scanned image rootfs for SBOM",
    );

    // Serialize. Both documents are deterministic (no wall-clock, no random
    // id), so re-running on the same image is byte-stable.
    let (media_type, doc) = match args.format {
        SbomFormat::Spdx => (
            SPDX_MEDIA_TYPE,
            to_spdx(&inventory.packages, args.reference, &entry.digest, &created),
        ),
        SbomFormat::Cyclonedx => (
            CYCLONEDX_MEDIA_TYPE,
            to_cyclonedx(&inventory.packages, args.reference, &entry.digest),
        ),
    };
    let bytes = serde_json::to_vec_pretty(&doc)?;

    // Output: explicit `--output` (file or `-`), else stdout unless attaching.
    match args.output {
        Some(out) if out == Path::new("-") => write_stdout(&bytes)?,
        Some(out) => {
            std::fs::write(out, &bytes)?;
            eprintln!("Wrote {} ({} bytes)", out.display(), bytes.len());
        }
        None if !args.attach => write_stdout(&bytes)?,
        None => {}
    }

    if args.attach {
        attach(&layout, &args, &entry, media_type, bytes)?;
    }

    eprintln!(
        "Generated {} SBOM: {} packages",
        format_label(args.format),
        inventory.packages.len(),
    );
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(bytes)?;
    out.write_all(b"\n")
}

/// Attach the generated document to the subject image as a referrer, pushing
/// it when asked.
fn attach(
    layout: &ImageLayout,
    args: &GenerateArgs<'_>,
    subject_entry: &oci_client::manifest::ImageIndexEntry,
    media_type: &str,
    bytes: Vec<u8>,
) -> Result<(), CliSbomError> {
    let subject = subject_from_entry(subject_entry);
    let blob = ArtifactBlob {
        media_type: media_type.to_string(),
        data: bytes::Bytes::from(bytes),
        annotations: None,
    };
    let referrer = emit_artifact_manifest(
        layout,
        media_type,
        Some(&subject),
        std::slice::from_ref(&blob),
        None,
        None,
    )?;
    println!(
        "Attached {media_type} SBOM {} -> {}",
        referrer.digest, subject.digest,
    );
    if args.push {
        push_referrer(
            layout,
            args.reference,
            args.username,
            args.password_stdin,
            args.insecure_registry,
            &referrer,
        )?;
    }
    Ok(())
}

/// The image config's `created` time (an RFC3339 string), or the Unix epoch
/// when the image records none. Reusing the image's own build time keeps the
/// SPDX document deterministic across re-runs (SPDX requires a `created`).
fn image_created(layout: &ImageLayout, config_digest: &str) -> String {
    layout
        .read_blob(config_digest)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("created").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

fn format_label(format: SbomFormat) -> &'static str {
    match format {
        SbomFormat::Spdx => "SPDX",
        SbomFormat::Cyclonedx => "CycloneDX",
    }
}

/// Build a minimal, valid, deterministic SPDX 2.3 document. The document
/// namespace folds the image digest (unique without a random id); `created`
/// reuses the image build time.
fn to_spdx(packages: &[Package], image_ref: &str, image_digest: &str, created: &str) -> Value {
    let pkgs: Vec<Value> = packages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let external_refs: Vec<Value> = p
                .purl
                .iter()
                .map(|purl| {
                    json!({
                        "referenceCategory": "PACKAGE-MANAGER",
                        "referenceType": "purl",
                        "referenceLocator": purl,
                    })
                })
                .collect();
            json!({
                "SPDXID": format!("SPDXRef-Package-{i}"),
                "name": p.name,
                "versionInfo": p.version,
                "downloadLocation": "NOASSERTION",
                "filesAnalyzed": false,
                "externalRefs": external_refs,
            })
        })
        .collect();
    let relationships: Vec<Value> = (0..packages.len())
        .map(|i| {
            json!({
                "spdxElementId": "SPDXRef-DOCUMENT",
                "relationshipType": "DESCRIBES",
                "relatedSpdxElement": format!("SPDXRef-Package-{i}"),
            })
        })
        .collect();
    json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": image_ref,
        "documentNamespace": format!("https://umf.imagilux.org/spdx/{image_digest}"),
        "creationInfo": {
            "created": created,
            "creators": [concat!("Tool: umf-", env!("CARGO_PKG_VERSION"))],
        },
        "packages": pkgs,
        "relationships": relationships,
    })
}

/// Build a minimal, valid, fully-deterministic CycloneDX 1.5 document (no
/// `serialNumber`, no `metadata.timestamp`).
fn to_cyclonedx(packages: &[Package], image_ref: &str, image_digest: &str) -> Value {
    let components: Vec<Value> = packages
        .iter()
        .map(|p| {
            let mut component = json!({
                "type": "library",
                "name": p.name,
                "version": p.version,
            });
            if let Some(purl) = &p.purl {
                component["purl"] = json!(purl);
            }
            component
        })
        .collect();
    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "container",
                "name": image_ref,
                "version": image_digest,
            },
        },
        "components": components,
    })
}

#[cfg(test)]
mod tests;
