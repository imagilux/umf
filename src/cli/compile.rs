//! `umf compile` — project a bootable-OS image into a local disk (block).
//!
//! `build` produces a plain OCI image; `compile` links it into a target disk.
//! The block is local-only: written to an explicit `-o PATH`, or to a
//! content-addressed sidecar in the layout's block cache (never an OCI artifact,
//! never pushed). The source image must already be in the layout — `umf build`
//! or `umf pull` it first.

use std::path::Path;

use thiserror::Error;
use tracing::info;
use umf_compile::{CompileError, DiskGeometry, compile_image};
use umf_oci::registry::{ImageLayout, RegistryError};

use super::util;

/// Errors from `umf compile`.
#[derive(Debug, Error)]
pub(crate) enum CliCompileError {
    /// Layout directory couldn't be resolved.
    #[error("cannot resolve layout directory: {0}")]
    LayoutDir(String),
    /// The reference isn't resident in the local layout.
    #[error(
        "{reference} is not in the local layout — `umf pull {reference}` or `umf build` it first"
    )]
    NotInLayout {
        /// The reference that was looked up.
        reference: String,
    },
    /// Layout / registry error.
    #[error("layout: {0}")]
    Registry(#[from] RegistryError),
    /// Projection error.
    #[error("compile: {0}")]
    Compile(#[from] CompileError),
}

/// Bundled `umf compile` flags.
pub(crate) struct CompileArgs<'a> {
    pub(crate) reference: &'a str,
    pub(crate) output: Option<&'a Path>,
    pub(crate) disk_size: Option<u64>,
    pub(crate) esp_size: Option<u64>,
    pub(crate) layout_dir_override: Option<&'a Path>,
}

pub(crate) fn run_compile(args: CompileArgs<'_>) -> Result<(), CliCompileError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliCompileError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;

    // The image must already be in the layout (built or pulled).
    let entry = layout
        .lookup_ref(args.reference)?
        .ok_or_else(|| CliCompileError::NotInLayout {
            reference: args.reference.to_string(),
        })?;

    // Defaults live with the geometry type in umf-compile so the block-cache
    // key derived below matches `umf run` / `umf save` byte-for-byte.
    let defaults = DiskGeometry::default();
    let geometry = DiskGeometry {
        disk_size_bytes: args.disk_size.unwrap_or(defaults.disk_size_bytes),
        esp_size_bytes: args.esp_size.unwrap_or(defaults.esp_size_bytes),
    };

    // Output: the explicit `-o PATH`, or a content-addressed block-cache sidecar
    // keyed on the source image digest + geometry. A cached block short-circuits
    // re-projection; the block is never an OCI artifact and can't be pushed.
    let (out, cached) = match args.output {
        Some(p) => (p.to_path_buf(), false),
        None => {
            let variant = geometry.cache_variant();
            let path = layout.block_cache_path(&entry.digest, &variant)?;
            let hit = path.is_file();
            (path, hit)
        }
    };

    if cached {
        info!(out = %out.display(), "block cache hit");
        println!(
            "Compiled {image} -> {out} (cached)",
            image = args.reference,
            out = out.display(),
        );
        return Ok(());
    }

    // The bootloader comes from the image (in-image `/usr/lib/systemd/boot/efi`)
    // for the classic flavor; there is no CLI override (the `compile_image`
    // override arg is a library test seam, so pass `None`).
    let report = compile_image(&layout, args.reference, &out, geometry, None)?;
    println!(
        "Compiled {image} -> {out}\n  \
         source: {digest}\n  \
         entrypoint: {ep}\n  \
         flavor: {flavor}\n  \
         disk: {size} bytes (ESP {esp})",
        image = args.reference,
        out = out.display(),
        digest = report.source_digest,
        ep = report.entrypoint,
        flavor = report.flavor,
        size = report.projection.disk_size_bytes,
        esp = report.projection.esp_size_bytes,
    );
    Ok(())
}
