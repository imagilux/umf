//! `umf index` — compose a multi-arch OCI image index from already-built
//! per-arch images in the local layout.
//!
//! The producer flow is one `umf build --platform=linux/<arch> --tag <ref>`
//! per architecture (each writes its image manifest + blobs into the shared
//! layout), then a single `umf index --tag <multi-ref> <amd64-ref>
//! <arm64-ref>` that stitches them into one
//! `application/vnd.oci.image.index.v1+json`. The emitted index pushes / pulls
//! like any other ref and `umf inspect --platform=…` selects a child from it.
//!
//! Each child ref must already resolve to a single-arch *image* manifest in the
//! layout; its `platform` descriptor is read from the child's own OCI config
//! (`architecture` / `os`), so the index never lies about what a child is.

use std::path::Path;

use oci_client::Reference;
use oci_client::manifest::OciManifest;
use thiserror::Error;
use tracing::info;
use umf_oci::image::{IndexChild, emit_index, platform_for};
use umf_oci::registry::{ImageLayout, RegistryError};

use crate::cli::util::{self, CredentialError};

#[derive(Debug, Error)]
pub(crate) enum CliIndexError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
    #[error("invalid OCI reference {reference:?}: {err}")]
    BadReference {
        reference: String,
        err: oci_client::ParseError,
    },
    #[error(
        "child ref not found in layout: {0} (build it first, e.g. `umf build --platform=… --tag {0}`)"
    )]
    ChildNotFound(String),
    #[error(
        "child ref {0} resolves to an image index, not a single-arch image; pass per-arch image refs, not another index"
    )]
    ChildIsIndex(String),
    #[error("child ref {reference} is missing an `architecture` in its OCI config")]
    ChildMissingArch { reference: String },
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest / config JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<CredentialError> for CliIndexError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf index` flags.
pub(crate) struct IndexArgs<'a> {
    /// Reference the composed index is registered under (`--tag`).
    pub(crate) tag: &'a str,
    /// One or more per-arch child image refs already present in the layout.
    pub(crate) children: &'a [String],
    /// Push the composed index (and its child trees) to the registry implied
    /// by `--tag` after writing it locally.
    pub(crate) push: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
}

/// Minimal view of a child image's OCI config — just the platform fields we
/// stamp into the index entry.
#[derive(Debug, Default, serde::Deserialize)]
struct ChildConfigPlatform {
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    os: Option<String>,
}

/// Compose the index from `--tag` + child refs, write it to the layout, and
/// optionally push it.
pub(crate) fn run_index(args: IndexArgs<'_>) -> Result<(), CliIndexError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliIndexError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;
    info!(layout = %layout_dir.display(), tag = %args.tag, "composing image index");

    let mut index_children = Vec::with_capacity(args.children.len());
    for child_ref in args.children {
        index_children.push(resolve_child(&layout, child_ref)?);
    }

    let entry = emit_index(&layout, &index_children, args.tag)?;
    println!(
        "Composed index {tag} -> {digest} ({n} arch{plural})",
        tag = args.tag,
        digest = entry.digest,
        n = index_children.len(),
        plural = if index_children.len() == 1 { "" } else { "es" },
    );

    if args.push {
        push_index(&layout, args)?;
    }
    Ok(())
}

/// Read a child ref's manifest + config from the layout and build its
/// [`IndexChild`] (digest + platform). Rejects refs that aren't a single-arch
/// image or that lack an `architecture`.
fn resolve_child(layout: &ImageLayout, child_ref: &str) -> Result<IndexChild, CliIndexError> {
    let entry = layout
        .lookup_ref(child_ref)?
        .ok_or_else(|| CliIndexError::ChildNotFound(child_ref.to_string()))?;
    let manifest_bytes = layout.read_blob(&entry.digest)?;

    let manifest = match serde_json::from_slice::<OciManifest>(&manifest_bytes)? {
        OciManifest::Image(image) => image,
        OciManifest::ImageIndex(_) => {
            return Err(CliIndexError::ChildIsIndex(child_ref.to_string()));
        }
    };

    let config_bytes = layout.read_blob(&manifest.config.digest)?;
    let platform: ChildConfigPlatform = serde_json::from_slice(&config_bytes).unwrap_or_default();
    let arch = platform
        .architecture
        .filter(|a| !a.is_empty())
        .ok_or_else(|| CliIndexError::ChildMissingArch {
            reference: child_ref.to_string(),
        })?;
    let os = platform
        .os
        .filter(|o| !o.is_empty())
        .unwrap_or_else(|| "linux".to_string());

    Ok(IndexChild {
        platform: platform_for(&os, &arch),
        manifest_digest: entry.digest,
    })
}

/// Push the just-composed index to the registry implied by `--tag`. The
/// registry client walks the index, pushing each child manifest tree before
/// the index itself.
fn push_index(layout: &ImageLayout, args: IndexArgs<'_>) -> Result<(), CliIndexError> {
    let reference: Reference =
        args.tag
            .parse()
            .map_err(|err: oci_client::ParseError| CliIndexError::BadReference {
                reference: args.tag.to_string(),
                err,
            })?;
    let credentials = util::credential_override(args.username, args.password_stdin)?;
    let auth = umf_oci::registry::auth::resolve_auth_for(Some(reference.registry()), &credentials);
    let client = util::registry_client_for(&reference, args.insecure_registry);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(client.push(&reference, args.tag, layout, &auth))?;
    println!("Pushed {}", args.tag);
    Ok(())
}
