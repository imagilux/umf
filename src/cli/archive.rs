//! `umf save / load` — round-trip refs between the local OCI layout
//! and an OCI Image Layout tarball (compatible with `skopeo copy
//! oci-archive:` and `docker save`).

use std::path::Path;

use thiserror::Error;
use umf_oci::registry::ImageLayout;

use crate::cli::util;

#[derive(Debug, Error)]
pub(crate) enum CliArchiveError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("archive: {0}")]
    Archive(#[from] umf_oci::archive::ArchiveError),
    #[error("layout: {0}")]
    Registry(#[from] umf_oci::registry::RegistryError),
    #[error("--type=block takes exactly one image reference (got {0})")]
    BlockNeedsOneRef(usize),
    #[error(
        "{reference} is `type={kind}` — `--type=block` extracts a disk only for a bootable image"
    )]
    NotBootable {
        /// The reference that was inspected.
        reference: String,
        /// The `org.imagilux.umf.type` value found.
        kind: String,
    },
    #[error(
        "no compiled block for {reference} in the cache — run `umf compile {reference}` (or \
         `umf run`) first to project it"
    )]
    NotCompiled {
        /// The reference with no cached block.
        reference: String,
    },
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// `umf save` — export `references` from the layout. Default: an OCI Image
/// Layout tarball (`-` for stdout). With `block`: extract the single ref's
/// compiled bootable disk from the block cache (a raw, `dd`-able image).
pub(crate) fn run_save(
    references: &[String],
    output: &Path,
    block: bool,
    layout_dir: Option<&Path>,
) -> Result<(), CliArchiveError> {
    let layout_dir = match layout_dir {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliArchiveError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;

    if block {
        return save_block(&layout, references, output);
    }

    if output == Path::new("-") {
        let stdout = std::io::stdout();
        let handle = stdout.lock();
        umf_oci::archive::save_to_writer(&layout, references, handle)?;
    } else {
        let file = std::fs::File::create(output)?;
        let buffered = std::io::BufWriter::new(file);
        umf_oci::archive::save_to_writer(&layout, references, buffered)?;
    }
    eprintln!(
        "Saved {} ref(s) to {}",
        references.len(),
        if output == Path::new("-") {
            "<stdout>".to_string()
        } else {
            output.display().to_string()
        },
    );
    Ok(())
}

/// Extract a bootable image's compiled disk from the block cache into `output`.
/// The block is a local-only sidecar (never an OCI artifact), so this is a raw
/// file copy — it must already have been projected by `umf compile` / `umf run`.
fn save_block(
    layout: &ImageLayout,
    references: &[String],
    output: &Path,
) -> Result<(), CliArchiveError> {
    let reference = match references {
        [r] => r.as_str(),
        other => return Err(CliArchiveError::BlockNeedsOneRef(other.len())),
    };

    // Only a bootable image has a disk to extract.
    let profile = umf_builder::introspect::introspect(layout, reference)?;
    if !matches!(profile.kind, umf_core::l0::L0Kind::Bootable) {
        return Err(CliArchiveError::NotBootable {
            reference: reference.to_string(),
            kind: profile.kind.to_string(),
        });
    }

    // Shared with `umf compile` / `umf run` so the cache key matches exactly
    // (the geometry + variant live in one place).
    let variant = umf_compile::DiskGeometry::default().cache_variant();
    let block = layout.block_cache_path(&profile.manifest_digest, &variant)?;
    if !block.is_file() {
        return Err(CliArchiveError::NotCompiled {
            reference: reference.to_string(),
        });
    }
    std::fs::copy(&block, output)?;
    eprintln!(
        "Extracted bootable block for {reference} -> {}",
        output.display(),
    );
    Ok(())
}

/// `umf load` — merge an OCI Image Layout tarball (`-` for stdin) into
/// the local layout, optionally overwriting colliding refs.
pub(crate) fn run_load(
    input: &Path,
    overwrite: bool,
    layout_dir: Option<&Path>,
) -> Result<(), CliArchiveError> {
    let layout_dir = match layout_dir {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliArchiveError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)
        .map_err(|e| CliArchiveError::Archive(umf_oci::archive::ArchiveError::Registry(e)))?;

    let loaded = if input == Path::new("-") {
        let stdin = std::io::stdin();
        let handle = stdin.lock();
        umf_oci::archive::load_from_reader(&layout, handle, overwrite)?
    } else {
        let file = std::fs::File::open(input)?;
        let buffered = std::io::BufReader::new(file);
        umf_oci::archive::load_from_reader(&layout, buffered, overwrite)?
    };

    if loaded.is_empty() {
        eprintln!("(archive carried no tagged refs to merge)");
    } else {
        eprintln!("Loaded {} ref(s):", loaded.len());
        for r in &loaded {
            eprintln!("  {r}");
        }
    }
    Ok(())
}
