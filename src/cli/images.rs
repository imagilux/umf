//! `umf images / push / pull` — on-disk OCI layout management:
//! listing cached refs, removing + pruning, and pushing / pulling
//! individual refs to / from the registry implied by their name.

use std::path::{Path, PathBuf};

use oci_client::Reference;
use oci_client::manifest::OciManifest;
use thiserror::Error;
use umf_oci::registry::auth::resolve_auth_for;
use umf_oci::registry::{ImageLayout, SearchRegistries, resolution_candidates};

use crate::cli::ImagesFormat;
use crate::cli::util::{self, CredentialError};

#[derive(Debug, Error)]
pub(crate) enum CliLayoutError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("registry: {0}")]
    Registry(#[from] umf_oci::registry::RegistryError),
    #[error("invalid OCI reference {reference:?}: {err}")]
    BadReference {
        reference: String,
        err: oci_client::ParseError,
    },
    #[error("ref not found in layout: {0}")]
    RefNotFound(String),
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<CredentialError> for CliLayoutError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Resolve the layout dir for layout-management subcommands, honouring
/// `--layout-dir` then falling back to the default cache location.
fn resolve_layout_dir(override_: Option<&Path>) -> Result<PathBuf, CliLayoutError> {
    match override_ {
        Some(p) => Ok(p.to_path_buf()),
        None => util::default_layout_dir().map_err(CliLayoutError::LayoutDir),
    }
}

/// Compact per-ref summary for `umf images` (list action).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ImagesRow {
    reference: String,
    digest: String,
    size_bytes: i64,
    umf_type: Option<String>,
}

/// Bundled args for `umf images`. The action (list / remove / prune)
/// is derived from which flags are populated:
///
/// - `remove` non-empty ⇒ remove (also runs prune if `prune` is set)
/// - `prune` true with empty `remove` ⇒ prune-only
/// - neither set, or `explicit_list` ⇒ list
pub(crate) struct ImagesArgs<'a> {
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) format: ImagesFormat,
    pub(crate) explicit_list: bool,
    pub(crate) remove: &'a [String],
    pub(crate) prune: bool,
}

/// Dispatch the `umf images` action implied by the populated flags.
pub(crate) fn run_images(args: ImagesArgs<'_>) -> Result<(), CliLayoutError> {
    let layout_dir = resolve_layout_dir(args.layout_dir_override)?;
    let layout = ImageLayout::init(&layout_dir)?;

    let removing = !args.remove.is_empty();
    let pruning_only = args.prune && !removing;
    let listing = args.explicit_list || (!removing && !args.prune);

    if removing {
        images_remove(&layout, args.remove, args.prune)?;
    } else if pruning_only {
        images_prune_only(&layout)?;
    }
    if listing {
        images_list(&layout, args.format)?;
    }
    Ok(())
}

/// The TYPE column for a layout ref. A multi-arch image index has no single
/// UMF type, so it reports as `index` rather than failing introspection (which
/// requires a single image — use `umf inspect --platform` to drill into a
/// child). Otherwise it is the introspected UMF kind, lowercased, or `None`
/// when the type can't be determined (corrupt / unreadable blob).
fn umf_type_for(layout: &ImageLayout, name: &str, digest: &str) -> Option<String> {
    if let Ok(bytes) = layout.read_blob(digest)
        && matches!(
            serde_json::from_slice::<OciManifest>(&bytes),
            Ok(OciManifest::ImageIndex(_)),
        )
    {
        return Some("index".to_string());
    }
    umf_builder::introspect::introspect(layout, name)
        .ok()
        .map(|p| format!("{:?}", p.kind).to_lowercase())
}

fn images_list(layout: &ImageLayout, format: ImagesFormat) -> Result<(), CliLayoutError> {
    let entries = layout.list_refs()?;

    // Build the rows — best-effort UMF-type lookup + real on-disk footprint
    // per ref. A failure (corrupt blob, missing config) leaves the type column
    // empty and falls the size back to the ref entry's own (manifest) size.
    let rows: Vec<ImagesRow> = entries
        .into_iter()
        .map(|(name, entry)| {
            let umf_type = umf_type_for(layout, &name, &entry.digest);
            let size_bytes = layout
                .image_disk_size(&entry.digest)
                .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
                .unwrap_or(entry.size);
            ImagesRow {
                reference: name,
                digest: entry.digest,
                size_bytes,
                umf_type,
            }
        })
        .collect();

    match format {
        ImagesFormat::Table => {
            if rows.is_empty() {
                println!("(no images in layout)");
                return Ok(());
            }
            println!(
                "{ref_col:<60} {type_col:<14} {size_col:>10}  {dig:.<24}",
                ref_col = "REFERENCE",
                type_col = "TYPE",
                size_col = "SIZE",
                dig = "DIGEST",
            );
            for r in &rows {
                println!(
                    "{ref_col:<60} {type_col:<14} {size_col:>10}  {dig}",
                    ref_col = util::truncate_for_column(&r.reference, 60),
                    type_col = r.umf_type.as_deref().unwrap_or("?"),
                    size_col = util::layout_human_bytes(r.size_bytes),
                    dig = util::truncate_chars(&r.digest, 24),
                );
            }
        }
        ImagesFormat::Json => {
            let json = serde_json::to_string_pretty(&rows)?;
            println!("{json}");
        }
    }
    Ok(())
}

fn images_remove(
    layout: &ImageLayout,
    references: &[String],
    prune: bool,
) -> Result<(), CliLayoutError> {
    let mut removed_any = false;
    for r in references {
        if layout.remove_ref(r)? {
            println!("Untagged: {r}");
            removed_any = true;
        } else {
            eprintln!("warning: ref not in layout: {r}");
        }
    }
    if prune {
        report_prune(layout)?;
    } else if removed_any {
        println!("(blobs not pruned; pass --prune to GC unreachable blobs)");
    }
    Ok(())
}

fn images_prune_only(layout: &ImageLayout) -> Result<(), CliLayoutError> {
    report_prune(layout)
}

fn report_prune(layout: &ImageLayout) -> Result<(), CliLayoutError> {
    let (n, bytes) = layout.prune_blobs()?;
    // Also GC the erofs lower-layer sidecar cache (keyed on diff_id).
    let (en, ebytes) = layout.prune_erofs_cache()?;
    // …and the bootable block cache (compiled disks, keyed on image digest) —
    // blocks whose source image is no longer in the layout are unreachable.
    let (bn, bbytes) = layout.prune_block_cache()?;
    if n == 0 && en == 0 && bn == 0 {
        println!("(no unreachable blobs)");
        return Ok(());
    }
    if n > 0 {
        println!(
            "Pruned {n} blob(s), {} freed",
            util::layout_human_bytes_u64(bytes),
        );
    }
    if en > 0 {
        println!(
            "Pruned {en} erofs cache file(s), {} freed",
            util::layout_human_bytes_u64(ebytes),
        );
    }
    if bn > 0 {
        println!(
            "Pruned {bn} block(s), {} freed",
            util::layout_human_bytes_u64(bbytes),
        );
    }
    Ok(())
}

/// `umf push` — upload an existing layout ref to its implied registry.
pub(crate) fn run_push_subcommand(
    reference: &str,
    insecure_registry: bool,
    username: Option<&str>,
    password_stdin: bool,
    layout_dir: Option<&Path>,
) -> Result<(), CliLayoutError> {
    let layout_dir = resolve_layout_dir(layout_dir)?;
    let layout = ImageLayout::init(&layout_dir)?;

    if layout.lookup_ref(reference)?.is_none() {
        return Err(CliLayoutError::RefNotFound(reference.to_string()));
    }

    let parsed_ref: Reference =
        reference
            .parse()
            .map_err(|err: oci_client::ParseError| CliLayoutError::BadReference {
                reference: reference.to_string(),
                err,
            })?;

    let credentials = util::credential_override(username, password_stdin)?;
    let auth = resolve_auth_for(Some(parsed_ref.registry()), &credentials);
    let client = util::registry_client_for(&parsed_ref, insecure_registry);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(client.push(&parsed_ref, reference, &layout, &auth))?;
    println!("Pushed {reference}");
    Ok(())
}

/// `umf pull` — fetch a ref from its implied registry into the layout.
pub(crate) fn run_pull_subcommand(
    reference: &str,
    insecure_registry: bool,
    username: Option<&str>,
    password_stdin: bool,
    layout_dir: Option<&Path>,
) -> Result<(), CliLayoutError> {
    let layout_dir = resolve_layout_dir(layout_dir)?;
    let layout = ImageLayout::init(&layout_dir)?;

    // Validate the reference up front so a malformed one reports clearly.
    let _: Reference =
        reference
            .parse()
            .map_err(|err: oci_client::ParseError| CliLayoutError::BadReference {
                reference: reference.to_string(),
                err,
            })?;

    // Expand an *unqualified* reference against the operator's search registries
    // (then docker.io), trying each in order. A qualified ref / empty list
    // yields just itself, so this is unchanged unless registries are configured.
    let search = SearchRegistries::load().search;
    let candidates = resolution_candidates(reference, &search);

    let credentials = util::credential_override(username, password_stdin)?;
    let rt = tokio::runtime::Runtime::new()?;
    let mut last_err: Option<umf_oci::registry::RegistryError> = None;

    for candidate in &candidates {
        let Ok(parsed_ref) = candidate.parse::<Reference>() else {
            continue;
        };
        let auth = resolve_auth_for(Some(parsed_ref.registry()), &credentials);
        let client = util::registry_client_for(&parsed_ref, insecure_registry);
        match rt.block_on(client.pull(&parsed_ref, &auth, &layout)) {
            Ok(entry) => {
                println!(
                    "Pulled {} -> {digest} ({} bytes)",
                    parsed_ref.whole(),
                    entry.size,
                    digest = entry.digest,
                );
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err
        .map(CliLayoutError::from)
        .unwrap_or_else(|| CliLayoutError::RefNotFound(reference.to_string())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use oci_client::manifest::ImageIndexEntry;

    /// A multi-arch image index reports TYPE `index` rather than failing
    /// introspection (which requires a single image) and rendering `?`.
    /// Regression test.
    #[test]
    fn umf_type_for_reports_index_for_multi_arch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = ImageLayout::init(dir.path()).expect("init");
        let index_bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": []
        }))
        .expect("index json");
        let digest = layout.write_blob(&index_bytes).expect("write index");
        layout
            .upsert_ref(
                "example.invalid/idx:1",
                ImageIndexEntry {
                    media_type: "application/vnd.oci.image.index.v1+json".to_string(),
                    digest: digest.clone(),
                    size: index_bytes.len() as i64,
                    platform: None,
                    annotations: None,
                },
            )
            .expect("upsert");
        assert_eq!(
            umf_type_for(&layout, "example.invalid/idx:1", &digest).as_deref(),
            Some("index"),
        );
    }

    /// A single-image ref still reports its introspected UMF kind, never
    /// `index` — a labelless layered image infers as `container`.
    #[test]
    fn umf_type_for_introspects_single_image() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = ImageLayout::init(dir.path()).expect("init");

        let cfg_bytes = serde_json::to_vec(&serde_json::json!({
            "architecture": "amd64", "os": "linux", "config": {},
            "rootfs": { "type": "layers", "diff_ids": [] }
        }))
        .expect("cfg json");
        let cfg_digest = layout.write_blob(&cfg_bytes).expect("write cfg");
        let layer_digest = layout.write_blob(b"a layer").expect("write layer");
        let m_bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": cfg_digest, "size": cfg_bytes.len() },
            "layers": [ { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                          "digest": layer_digest, "size": 7 } ]
        }))
        .expect("manifest json");
        let m_digest = layout.write_blob(&m_bytes).expect("write manifest");
        layout
            .upsert_ref(
                "example.invalid/single:1",
                ImageIndexEntry {
                    media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                    digest: m_digest.clone(),
                    size: m_bytes.len() as i64,
                    platform: None,
                    annotations: None,
                },
            )
            .expect("upsert");
        assert_eq!(
            umf_type_for(&layout, "example.invalid/single:1", &m_digest).as_deref(),
            Some("container"),
        );
    }
}
