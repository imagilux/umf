//! OCI bundle preparation.
//!
//! A [`Bundle`] is what a runc-compatible runtime expects: a directory
//! containing `rootfs/` (the unpacked layer stack) and `config.json` (an
//! OCI runtime-spec document). The bundle is staged in a [`TempDir`] that
//! is deleted when the [`Bundle`] is dropped, so partial builds leave
//! nothing behind.
//!
//! ## Pipeline
//!
//! 1. Resolve `ref_name` against the [`ImageLayout`] → manifest blob.
//! 2. Walk the manifest's layer list, unpacking each gzipped tar into
//!    `rootfs/` in order. OCI whiteouts (`.wh.foo`, `.wh..wh..opq`) are
//!    applied so the final tree matches what the runtime sees.
//! 3. Read the image-config to recover environment, working-dir, and
//!    entrypoint defaults — these populate the OCI runtime-spec's
//!    `process` block.
//! 4. Build an OCI runtime-spec [`oci_spec::runtime::Spec`] with the
//!    rootless namespace set, uid/gid mappings, and a Linux-shaped
//!    capability profile.
//! 5. Serialise the spec to `config.json` next to `rootfs/`.
//!
//! ## What's not here
//!
//! - **Overlay setup.** The bundle's `rootfs` is a flat directory.
//!   Backends that want RUN-step writes to land in a captured upper-dir
//!   (so they can be packaged as a layer) wrap the rootfs with an
//!   [`crate::overlay::Overlay`] before running. Bundle prep stays
//!   target-agnostic.
//! - **Cache reuse.** Each call re-unpacks the layers into a fresh
//!   tempdir. Caching is a future optimisation.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use oci_client::manifest::{OciImageManifest, OciManifest};
use oci_spec::runtime::{
    Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxIdMapping, LinuxIdMappingBuilder,
    LinuxNamespaceBuilder, LinuxNamespaceType, LinuxPidsBuilder, LinuxResources,
    LinuxResourcesBuilder, MountBuilder, ProcessBuilder, RootBuilder, Spec, SpecBuilder,
    UserBuilder, get_default_maskedpaths, get_default_readonly_paths,
};
use serde::Deserialize;
use tempfile::TempDir;
use tracing::{debug, warn};
use umf_oci::registry::ImageLayout;

use crate::erofs::MountedErofsLayers;
use crate::error::EngineError;

/// How the base image's layers are materialised into the read-only lower
/// stack that the build overlays on top of.
///
/// The `#[derive(Default)]` value is the conservative [`Self::Merge`]
/// (no erofs dependency, always works). That governs only callers that
/// reach for [`Default`] — e.g. [`BundleOptions::default`]. The runtime
/// selector [`Self::from_env`] is separate and instead *prefers*
/// [`Self::Erofs`] (falling back to merge when the host can't mount it);
/// that's what both `umf build` and `umf run` consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerStrategy {
    /// Unpack every layer (merged, with OCI whiteouts applied) into a
    /// single `rootfs/` directory. The lower stack is that one directory.
    /// The conservative default and the erofs fallback; also forced by
    /// `umf run --keep-bundle`, which needs an inspectable on-disk rootfs.
    #[default]
    Merge,
    /// Encode each layer as a content-addressed erofs image (cached in
    /// the layout) and mount them read-only as a stacked lower set —
    /// skips the per-build unpack. Silently falls back to
    /// [`Self::Merge`] when the host can't encode/mount erofs.
    Erofs,
}

impl LayerStrategy {
    /// Pick the strategy from the `UMF_LAYER_CACHE` environment variable
    /// (`auto` | `erofs` | `unpack`), the single source of truth shared
    /// by `umf build` and `umf run`. `auto` (default) and `erofs` request
    /// [`Self::Erofs`] — `from_image` still falls back to [`Self::Merge`]
    /// when the host can't encode/mount erofs. `unpack` forces
    /// [`Self::Merge`]. An unrecognised value warns and uses `auto`.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("UMF_LAYER_CACHE").as_deref().map(str::trim) {
            Ok("unpack") => Self::Merge,
            Ok("erofs" | "auto" | "") | Err(_) => Self::Erofs,
            Ok(other) => {
                tracing::warn!(
                    value = other,
                    "ignoring unknown UMF_LAYER_CACHE value; using auto (erofs when available)"
                );
                Self::Erofs
            }
        }
    }
}

/// Per-bundle options the caller can override.
#[derive(Debug, Clone)]
pub struct BundleOptions {
    /// Container hostname (`hostname` field in the runtime spec).
    pub hostname: String,
    /// Whether the spec should ask youki to create a user namespace (+ uid/gid
    /// mappings). True only when we are unprivileged **and** did not enter our
    /// own namespace first (the library-consumer fallback). After
    /// [`crate::rootless::enter`] we own one, so this is `false` and youki runs
    /// rootful inside it, nesting nothing; `false` for real root too.
    pub rootless: bool,
    /// Host uid that container uid 0 maps to (the unprivileged build user).
    pub host_uid: u32,
    /// Host gid that container gid 0 maps to (the unprivileged build user).
    pub host_gid: u32,
    /// Whether the process holds **real** host privilege (started as uid 0 in
    /// the initial user namespace). Distinct from `!rootless`: after entering
    /// our own user namespace the euid is 0 without host authority, so
    /// host-only steps (cgroup resource limits, erofs mounts) gate on this, not
    /// on the euid. Defaults from [`crate::rootless::context`].
    pub host_privileged: bool,
    /// How to materialise the base image's lower layers. Defaults to
    /// [`LayerStrategy::Merge`] (today's unpack behaviour). The builder
    /// requests [`LayerStrategy::Erofs`] when the host supports it.
    pub layer_strategy: LayerStrategy,
    /// Target CPU architecture. When the image is a multi-arch index,
    /// [`Bundle::from_image`] selects the manifest matching this arch
    /// (and errors if the index doesn't publish it) rather than assuming
    /// amd64. Defaults to the build host's architecture.
    pub architecture: umf_core::architecture::Architecture,
}

impl BundleOptions {
    /// Build options for the current host process. Derives the rootless
    /// shape (user namespace plus uid/gid mappings) from the running
    /// process identity, exactly as [`BundleOptions::default`], while
    /// letting the caller set the hostname, layer strategy, and target
    /// architecture.
    ///
    /// Build call sites must construct host options through this rather
    /// than hardcoding `rootless`/`host_uid`/`host_gid`. An unprivileged
    /// `umf build` (uid != 0) then gets a valid user-namespace spec instead
    /// of a root-assuming one that libcontainer rejects.
    pub fn for_host(
        hostname: impl Into<String>,
        layer_strategy: LayerStrategy,
        architecture: umf_core::architecture::Architecture,
    ) -> Self {
        Self {
            hostname: hostname.into(),
            layer_strategy,
            architecture,
            ..Self::default()
        }
    }
}

impl Default for BundleOptions {
    fn default() -> Self {
        // Derive the rootless shape from the process-wide context established
        // at startup by `rootless::enter` (or its passive fallback when the
        // engine is driven without the CLI hook). `host_uid`/`host_gid` are the
        // original build-user ids — preserved there because `getuid()` reports
        // 0 once we are inside our own user namespace.
        let ctx = crate::rootless::context();
        Self {
            hostname: "umf-build".to_string(),
            rootless: !ctx.host_privileged && !ctx.entered_userns,
            host_uid: ctx.host_uid,
            host_gid: ctx.host_gid,
            host_privileged: ctx.host_privileged,
            layer_strategy: LayerStrategy::default(),
            architecture: umf_core::architecture::Architecture::default(),
        }
    }
}

/// A prepared OCI bundle on disk.
///
/// Owns a [`TempDir`]; dropping the bundle removes everything.
#[derive(Debug)]
pub struct Bundle {
    /// Drop guard for the bundle's working directory.
    _tempdir: TempDir,
    /// Absolute path to the bundle directory (contains `rootfs/` and
    /// `config.json`).
    path: PathBuf,
    /// Absolute path to the rootfs directory inside the bundle.
    rootfs: PathBuf,
    /// In-memory mutable copy of the runtime spec. Backends mutate this
    /// (to install per-RUN argv / env / cwd) and then call
    /// [`Self::write_spec`] to flush to `config.json`.
    spec: Spec,
    /// The image-config's `Entrypoint` field as pulled from `config.json`.
    /// Kept verbatim so the run-path can distinguish "this came from the
    /// image" from "the caller overrode it" when applying CLI flags.
    image_entrypoint: Vec<String>,
    /// The image-config's `Cmd` field. Same rationale as `image_entrypoint`.
    image_cmd: Vec<String>,
    /// The base image's read-only lower stack, **top → bottom** (newest
    /// layer first). For [`LayerStrategy::Merge`] this is the single
    /// merged `rootfs` directory; for [`LayerStrategy::Erofs`] it's the
    /// per-layer erofs mountpoints. The builder feeds this to its
    /// overlay (see `umf-builder`'s `lower_stack`).
    base_lowers: Vec<PathBuf>,
    /// Drop guard for any mounted erofs layers. `None` under
    /// [`LayerStrategy::Merge`]. Held so the mountpoints in `base_lowers`
    /// stay live for the bundle's lifetime and are unmounted on drop.
    _erofs: Option<MountedErofsLayers>,
}

impl Bundle {
    /// Prepare a bundle from a pulled image already resident in `layout`.
    ///
    /// # Errors
    /// - [`EngineError::ImageNotInLayout`] if `ref_name` isn't indexed.
    /// - [`EngineError::Json`] / [`EngineError::Registry`] if the
    ///   layout's manifest, config, or layer blobs are malformed.
    /// - [`EngineError::LayerCountMismatch`] if the manifest's layer
    ///   count disagrees with the image-config's `rootfs.diff_ids`.
    /// - [`EngineError::UnsupportedLayerMediaType`] for layer
    ///   media-types we don't know how to unpack.
    /// - [`EngineError::Io`] for filesystem errors during the unpack.
    #[tracing::instrument(
        level = "info",
        name = "umf.engine.bundle.prepare",
        skip(layout, options),
        fields(
            ref_name = %ref_name,
            rootless = options.rootless,
        )
    )]
    pub fn from_image(
        layout: &ImageLayout,
        ref_name: &str,
        options: &BundleOptions,
    ) -> Result<Self, EngineError> {
        let entry = layout
            .lookup_ref(ref_name)?
            .ok_or_else(|| EngineError::ImageNotInLayout(ref_name.to_string()))?;

        // Resolve image-index to a single-image manifest if necessary,
        // selecting the manifest matching `options.architecture`
        // (`--platform`). `oci_arch_string` yields the OCI shorthand that
        // `Arch::from(&str)` maps to the matching variant. No match is an
        // error: falling back to an arbitrary manifest would unpack the
        // wrong arch.
        use oci_spec::image::{Arch, Os};
        let want_arch = Arch::from(options.architecture.oci_arch_string());
        let manifest_bytes = layout.read_blob(&entry.digest)?;
        let manifest: OciImageManifest =
            match serde_json::from_slice::<OciManifest>(&manifest_bytes)? {
                OciManifest::Image(image) => image,
                OciManifest::ImageIndex(index) => {
                    let chosen = index
                        .manifests
                        .iter()
                        .find(|m| {
                            m.platform.as_ref().is_some_and(|p| {
                                p.os == Os::Linux
                                    && p.architecture == want_arch
                                    && p.variant.as_deref().is_none_or(|v| v.is_empty())
                            })
                        })
                        .or_else(|| {
                            // Same arch, any variant.
                            index.manifests.iter().find(|m| {
                                m.platform.as_ref().is_some_and(|p| {
                                    p.os == Os::Linux && p.architecture == want_arch
                                })
                            })
                        })
                        .ok_or_else(|| EngineError::NoManifestForPlatform {
                            arch: options.architecture.oci_arch_string().to_string(),
                        })?;
                    let child_bytes = layout.read_blob(&chosen.digest)?;
                    serde_json::from_slice(&child_bytes)?
                }
            };
        let config_bytes = layout.read_blob(&manifest.config.digest)?;
        let image_config: ImageConfigDoc = serde_json::from_slice(&config_bytes)?;

        let diff_ids_len = image_config.rootfs.diff_ids.len();
        if diff_ids_len != manifest.layers.len() {
            return Err(EngineError::LayerCountMismatch {
                layers: manifest.layers.len(),
                diff_ids: diff_ids_len,
            });
        }

        // Stage the bundle.
        let tempdir = TempDir::new()?;
        let path = tempdir.path().to_path_buf();
        let rootfs = path.join("rootfs");
        fs::create_dir_all(&rootfs)?;

        // Materialise the base image's lower stack per the requested
        // strategy. Erofs is an optional acceleration — any failure (or an
        // unsupported host) falls back to the merged unpack so the build
        // always succeeds.
        let want_erofs = options.layer_strategy == LayerStrategy::Erofs
            && !manifest.layers.is_empty()
            // `mkfs.erofs --gzip` only decodes gzip layers; a zstd layer must
            // take the merged-unpack path instead (which fingerprints the codec
            // and zstd-decodes). Gate erofs on every layer being gzip-encodable
            // so a single zstd layer cleanly opts the whole image out of the
            // erofs acceleration rather than being mis-decoded as gzip.
            && manifest
                .layers
                .iter()
                .all(|d| is_erofs_encodable_media_type(&d.media_type))
            && umf_oci::erofs::encoder_available()
            && crate::erofs::mount_available();

        let (base_lowers, erofs_guard): (Vec<PathBuf>, Option<MountedErofsLayers>) = if want_erofs {
            match Self::mount_erofs_lowers(layout, &manifest, &image_config) {
                Ok((lowers, mounted)) => {
                    debug!(layers = lowers.len(), "bundle base lowers via erofs");
                    (lowers, Some(mounted))
                }
                Err(e) => {
                    warn!(error = %e, "erofs lower setup failed; falling back to unpack");
                    unpack_layers_into(layout, &manifest, &rootfs)?;
                    (vec![rootfs.clone()], None)
                }
            }
        } else {
            unpack_layers_into(layout, &manifest, &rootfs)?;
            (vec![rootfs.clone()], None)
        };

        // Build the runtime spec.
        let spec = build_runtime_spec(&rootfs, &image_config, options)?;

        // Write config.json.
        let config_path = path.join("config.json");
        let config_json = serde_json::to_vec_pretty(&spec).map_err(EngineError::SerialiseSpec)?;
        fs::write(&config_path, &config_json)?;

        Ok(Self {
            _tempdir: tempdir,
            path,
            rootfs,
            spec,
            image_entrypoint: image_config.config.entrypoint.clone(),
            image_cmd: image_config.config.cmd.clone(),
            base_lowers,
            _erofs: erofs_guard,
        })
    }

    /// Stage a bundle for a `FROM scratch` build: an empty rootfs with no
    /// base image involved.
    ///
    /// The single empty rootfs directory doubles as the one base lower —
    /// the same shape [`Self::from_image`]'s unpack path produces — so
    /// overlay stacking and RUN execution work unchanged: a RUN against
    /// the still-empty filesystem fails at exec exactly like docker's
    /// `FROM scratch` does, while a RUN after an `ADD` sees the added
    /// files through the overlay.
    pub fn from_scratch(options: &BundleOptions) -> Result<Self, EngineError> {
        let tempdir = TempDir::new()?;
        let path = tempdir.path().to_path_buf();
        let rootfs = path.join("rootfs");
        fs::create_dir_all(&rootfs)?;

        let image_config = ImageConfigDoc::default();
        let spec = build_runtime_spec(&rootfs, &image_config, options)?;
        let config_path = path.join("config.json");
        let config_json = serde_json::to_vec_pretty(&spec).map_err(EngineError::SerialiseSpec)?;
        fs::write(&config_path, &config_json)?;

        Ok(Self {
            _tempdir: tempdir,
            path,
            rootfs: rootfs.clone(),
            spec,
            image_entrypoint: image_config.config.entrypoint.clone(),
            image_cmd: image_config.config.cmd.clone(),
            base_lowers: vec![rootfs],
            _erofs: None,
        })
    }

    /// Encode every base layer to erofs (idempotent, cached) and mount
    /// the images read-only as a stacked lower set, **top → bottom**
    /// (newest layer first — the order overlayfs expects).
    ///
    /// Returns the mountpoint paths plus the [`MountedErofsLayers`] drop
    /// guard that owns them.
    fn mount_erofs_lowers(
        layout: &ImageLayout,
        manifest: &OciImageManifest,
        image_config: &ImageConfigDoc,
    ) -> Result<(Vec<PathBuf>, MountedErofsLayers), EngineError> {
        // Encode in manifest (oldest → newest) order, validating media
        // types as we go (erofs --gzip expects a gzipped tar layer; zstd
        // layers are filtered out earlier by the `want_erofs` gate, but
        // re-check here so this path never feeds mkfs.erofs a codec it can't
        // decode).
        let mut erofs_paths = Vec::with_capacity(manifest.layers.len());
        for (descriptor, diff_id) in manifest.layers.iter().zip(&image_config.rootfs.diff_ids) {
            if !is_erofs_encodable_media_type(&descriptor.media_type) {
                return Err(EngineError::UnsupportedLayerMediaType(
                    descriptor.media_type.clone(),
                ));
            }
            let p = umf_oci::erofs::ensure_layer_erofs(layout, &descriptor.digest, diff_id)?;
            erofs_paths.push(p);
        }
        // Overlay lowers are top → bottom = newest → oldest.
        erofs_paths.reverse();
        let mounted = MountedErofsLayers::mount(&erofs_paths)?;
        let lowers = mounted
            .mountpoints()
            .iter()
            .map(|p| p.to_path_buf())
            .collect();
        Ok((lowers, mounted))
    }

    /// The image-config's `Entrypoint` field as parsed from the manifest's
    /// config blob. Read-only; already applied to the runtime spec at
    /// bundle creation. The run-path uses this to distinguish "image
    /// default" from "caller override" when constructing the final argv.
    #[must_use]
    pub fn image_entrypoint(&self) -> &[String] {
        &self.image_entrypoint
    }

    /// The image-config's `Cmd` field. Same rationale as
    /// [`Self::image_entrypoint`].
    #[must_use]
    pub fn image_cmd(&self) -> &[String] {
        &self.image_cmd
    }

    /// Take ownership of the bundle's `TempDir`, returning the absolute
    /// path. After this call, the on-disk bundle is no longer
    /// auto-cleaned — the caller is responsible for the directory's
    /// lifecycle. Used by [`crate::run::run_image`] to implement
    /// `--keep-bundle`.
    #[must_use]
    pub fn into_persistent(self) -> PathBuf {
        let path = self.path.clone();
        // Intentional leak: relinquish the TempDir's drop-time cleanup
        // so the directory survives. The user asked for it; they own
        // the cleanup.
        std::mem::forget(self._tempdir);
        path
    }

    /// Absolute path to the bundle root (contains `rootfs/` + `config.json`).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Absolute path to the bundle's rootfs directory.
    ///
    /// Under [`LayerStrategy::Erofs`] this directory is empty — the base
    /// image lives in the erofs lowers ([`Self::base_lowers`]) instead.
    #[must_use]
    pub fn rootfs(&self) -> &Path {
        &self.rootfs
    }

    /// The base image's read-only lower stack, **top → bottom** (newest
    /// layer first). Either the single merged `rootfs` directory
    /// ([`LayerStrategy::Merge`]) or the per-layer erofs mountpoints
    /// ([`LayerStrategy::Erofs`]). Callers stack their own RUN/ADD
    /// upper-dirs on top of these to form the overlay lower list.
    #[must_use]
    pub fn base_lowers(&self) -> Vec<&Path> {
        self.base_lowers.iter().map(PathBuf::as_path).collect()
    }

    /// Whether the base image was materialised as mounted erofs layers
    /// ([`LayerStrategy::Erofs`]) rather than a merged `rootfs` directory.
    /// When `true`, `base_lowers()` are **read-only** mounts — a consumer
    /// that needs a writable root (e.g. `umf run`) must wrap them in an
    /// overlay with its own upper rather than writing to them directly.
    #[must_use]
    pub fn uses_erofs_lowers(&self) -> bool {
        self._erofs.is_some()
    }

    /// The current runtime spec (in-memory, may not match `config.json`
    /// on disk if [`Self::write_spec`] hasn't been called since the
    /// last mutation).
    #[must_use]
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Mutable handle to the runtime spec. Backends use this to install
    /// per-RUN process args / env / cwd / user before launching the
    /// container, then call [`Self::write_spec`].
    pub fn spec_mut(&mut self) -> &mut Spec {
        &mut self.spec
    }

    /// Flush the in-memory spec to `config.json` on disk.
    ///
    /// # Errors
    /// JSON serialisation or filesystem write failure.
    pub fn write_spec(&self) -> Result<(), EngineError> {
        let config_path = self.path.join("config.json");
        let bytes = serde_json::to_vec_pretty(&self.spec).map_err(EngineError::SerialiseSpec)?;
        fs::write(config_path, bytes)?;
        Ok(())
    }

    /// Override the runtime spec's `root.path` to point at `path` instead
    /// of the bundle's own unpacked rootfs.
    ///
    /// Used when the caller wraps the rootfs in an overlay (or any other
    /// stacking layer) before handing the bundle to a runtime backend.
    /// Callers should follow this with [`Self::write_spec`] to flush the
    /// change before the runtime reads `config.json`.
    ///
    /// # Errors
    /// `RootBuilder` rejects the path (should not happen with any valid
    /// path; surfaced for safety).
    pub fn set_root_path(&mut self, path: &Path) -> Result<(), EngineError> {
        use oci_spec::runtime::RootBuilder;
        let root = RootBuilder::default()
            .path(path.to_path_buf())
            .readonly(false)
            .build()
            .map_err(|e| {
                EngineError::runtime(
                    format!("RootBuilder rejected `{}`: {e}", path.display()),
                    None,
                )
            })?;
        self.spec.set_root(Some(root));
        Ok(())
    }
}

// ── Layer unpacking with OCI whiteout support ───────────────────────────────

/// OCI image-spec layer media-types we know how to unpack.
///
/// Decompression is delegated to [`umf_oci::materialize::apply_layer`], which
/// fingerprints the codec from the blob's leading magic, so adding a media type
/// here only requires that `apply_layer` can decode the matching bytes (gzip +
/// zstd today). The erofs lower path additionally needs the encoder to accept
/// the codec, but it shares this gate; erofs is an optional acceleration that
/// falls back to the merged unpack, so an unsupported-for-erofs codec is
/// handled by that fallback, not a hard gate change.
fn is_supported_layer_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        umf_oci::image::IMAGE_LAYER_ZSTD_MEDIA_TYPE
            | oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE
            | "application/vnd.docker.image.rootfs.diff.tar.gzip"
    )
}

/// Layer media-types the erofs lower path can encode directly.
///
/// Narrower than [`is_supported_layer_media_type`]: `mkfs.erofs --tar=f --gzip`
/// only consumes gzip-compressed tar layers, so zstd layers are excluded here
/// and routed through the merged-unpack path instead (see the `want_erofs`
/// gate). The Docker gzip alias is encodable for the same reason gzip is.
fn is_erofs_encodable_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE
            | "application/vnd.docker.image.rootfs.diff.tar.gzip"
    )
}

/// Unpack every layer of `manifest` into `dst` in manifest order
/// (oldest base layer first), applying OCI whiteouts so the merged tree
/// matches what a runtime would see. This is the [`LayerStrategy::Merge`]
/// path (and the erofs fallback).
fn unpack_layers_into(
    layout: &ImageLayout,
    manifest: &OciImageManifest,
    dst: &Path,
) -> Result<(), EngineError> {
    for descriptor in &manifest.layers {
        if !is_supported_layer_media_type(&descriptor.media_type) {
            return Err(EngineError::UnsupportedLayerMediaType(
                descriptor.media_type.clone(),
            ));
        }
        let blob = layout.read_blob(&descriptor.digest)?;
        unpack_layer_into(&blob, dst)?;
        debug!(
            "unpacked layer {} ({} bytes) into bundle rootfs",
            descriptor.digest,
            blob.len(),
        );
    }
    Ok(())
}

/// Unpack `blob` (a gzipped tar) into `dst`, applying OCI whiteouts.
///
/// Delegates to `umf_oci::materialize::apply_layer` — the single,
/// containment-checked implementation of OCI layer extraction and whiteout
/// handling (`.wh.<name>` removals and `.wh..wh..opq` opaque-directory clears),
/// shared with the `umf compile` projection path so the two can no longer drift
/// (a containment fix lands in exactly one place). This crate keeps only the
/// media-type gate, in [`unpack_layers_into`].
fn unpack_layer_into(blob: &[u8], dst: &Path) -> Result<(), EngineError> {
    umf_oci::materialize::apply_layer(blob, dst).map_err(EngineError::from)
}

// ── OCI runtime spec construction ───────────────────────────────────────────

/// Build a sensible default runtime spec.
///
/// The spec is intentionally conservative — capabilities are limited to
/// the AUDIT/CHOWN/DAC/FOWNER/FSETID/KILL/MKNOD/NET_BIND/NET_RAW/SETFCAP
/// /SETGID/SETPCAP/SETUID/SYS_CHROOT bounding set (the conventional
/// container-runtime default). No CAP_SYS_ADMIN, no SYS_MODULE, no
/// SYS_TIME. `noNewPrivileges` is on. The rootless library-consumer path
/// includes a user namespace with the caller's full subordinate-id map (host
/// uid → container `0`, plus the delegated range); the rootful path drops the
/// user namespace and assumes the runtime supplies root privileges.
fn build_runtime_spec(
    rootfs: &Path,
    image_config: &ImageConfigDoc,
    options: &BundleOptions,
) -> Result<Spec, EngineError> {
    let env = if image_config.config.env.is_empty() {
        vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()]
    } else {
        image_config.config.env.clone()
    };
    let cwd = if image_config.config.working_dir.is_empty() {
        "/".to_string()
    } else {
        image_config.config.working_dir.clone()
    };

    // The args populated here are placeholders — backends overwrite them
    // via `Bundle::spec_mut()` before each RUN step. We seed them from the
    // image's Entrypoint+Cmd so a bundle is still runnable as-is (useful
    // for `bundle.spec().process()` smoke checks).
    let args = if image_config.config.entrypoint.is_empty() && image_config.config.cmd.is_empty() {
        vec!["/bin/sh".to_string()]
    } else {
        let mut combined = image_config.config.entrypoint.clone();
        combined.extend(image_config.config.cmd.iter().cloned());
        combined
    };

    let user = UserBuilder::default()
        .uid(0_u32)
        .gid(0_u32)
        .build()
        .map_err(spec_build_error)?;

    let caps_default = default_caps();
    // Evaluate the seccomp profile's cap/arch gates against this exact
    // capability set before it is moved into the capabilities builder — the
    // gate result depends on which caps the RUN process actually holds.
    let seccomp_profile = crate::seccomp::filtered_profile(&caps_default, options.architecture)?;
    let capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(caps_default.clone())
        .effective(caps_default.clone())
        .inheritable(caps_default.clone())
        .permitted(caps_default.clone())
        .ambient(caps_default)
        .build()
        .map_err(spec_build_error)?;

    // Optional LSM (AppArmor / SELinux) confinement — a defence-in-depth
    // backstop applied only when the host supports it and a profile/label is
    // available; otherwise every field is `None` and the spec is unchanged.
    let lsm = crate::lsm::LsmConfig::detect();
    let mut process_builder = ProcessBuilder::default()
        .terminal(false)
        .user(user)
        .args(args)
        .env(env)
        .cwd(cwd)
        .capabilities(capabilities)
        .no_new_privileges(true);
    if let Some(profile) = lsm.apparmor_profile.clone() {
        process_builder = process_builder.apparmor_profile(profile);
    }
    if let Some(label) = lsm.selinux_process_label.clone() {
        process_builder = process_builder.selinux_label(label);
    }
    let process = process_builder.build().map_err(spec_build_error)?;

    let root = RootBuilder::default()
        .path(rootfs)
        .readonly(false)
        .build()
        .map_err(spec_build_error)?;

    let mounts = standard_mounts()?;

    let mut namespaces = vec![
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Pid)
            .build()
            .map_err(spec_build_error)?,
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Network)
            .build()
            .map_err(spec_build_error)?,
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Ipc)
            .build()
            .map_err(spec_build_error)?,
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Uts)
            .build()
            .map_err(spec_build_error)?,
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Mount)
            .build()
            .map_err(spec_build_error)?,
        // Cgroup namespace: give the RUN step its own cgroup-hierarchy root so
        // it can't read the host's cgroup layout via /proc/self/cgroup.
        // Unsharing CLONE_NEWCGROUP is permitted unprivileged (Linux >= 4.6).
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Cgroup)
            .build()
            .map_err(spec_build_error)?,
    ];
    let mut linux_builder = LinuxBuilder::default();
    if options.rootless {
        namespaces.push(
            LinuxNamespaceBuilder::default()
                .typ(LinuxNamespaceType::User)
                .build()
                .map_err(spec_build_error)?,
        );
        // The two-entry sub-id map (see `rootless_id_mappings`) so a base
        // image's non-root-owned files and `RUN --user <nonzero>` resolve to
        // real ids. There is no single-id fallback: a missing delegation is a
        // hard, actionable error rather than a silent degrade to `nobody`.
        let (uid_mappings, gid_mappings) =
            rootless_id_mappings(options.host_uid, options.host_gid)?;
        linux_builder = linux_builder
            .uid_mappings(uid_mappings)
            .gid_mappings(gid_mappings);
    }

    // Bound runaway RUN steps (e.g. an accidental fork-bomb in untrusted build
    // instructions) with a cgroup pids cap. Emitted unconditionally: the
    // real-root build's fs cgroup manager writes `pids.max`, a rootless build
    // that entered its own user namespace gets it as `TasksMax` on the
    // user-delegated scope, and even the legacy library-consumer path that asks
    // youki for the user namespace now carries the limit so any runtime with
    // pids-controller delegation enforces it. `LinuxResources` is advisory — a
    // host without pids delegation simply gets no ceiling (best-effort), never
    // a hard failure — so emitting it always fails safe.
    linux_builder = linux_builder.resources(build_run_resources()?);

    // Attach the gate-evaluated containerd/Docker default seccomp profile
    // (computed above from `caps_default` + target arch): deny-by-default with a
    // large safe-syscall allowlist, so a RUN step can't reach the dangerous tail
    // of the host syscall surface (`kexec_load`, `bpf`, arbitrary `ptrace`,
    // `unshare`/`setns`/`mount` — all gated behind caps this container never
    // holds). Applied for both root and rootless builds. libcontainer enforces
    // it via its `libseccomp` feature (enabled in the workspace `Cargo.toml`).
    linux_builder = linux_builder.seccomp(seccomp_profile);

    // SELinux mount label for the container rootfs (enforcing hosts only).
    if let Some(label) = lsm.selinux_mount_label.clone() {
        linux_builder = linux_builder.mount_label(label);
    }

    let linux = linux_builder
        .namespaces(namespaces)
        // Mask + read-only the conventional /proc and /sys paths a sandboxed
        // process must not read or tamper with (kcore, keys, sysrq-trigger,
        // firmware, ...). These are the standard runc/crun defaults; the
        // hand-built spec previously shipped neither, so a RUN step saw an
        // unmasked /proc and a writable /proc/sysrq-trigger.
        .masked_paths(get_default_maskedpaths())
        .readonly_paths(get_default_readonly_paths())
        .build()
        .map_err(spec_build_error)?;

    let spec = SpecBuilder::default()
        .version("1.0.2")
        .hostname(options.hostname.clone())
        .root(root)
        .process(process)
        .mounts(mounts)
        .linux(linux)
        .build()
        .map_err(spec_build_error)?;
    Ok(spec)
}

fn spec_build_error<E: std::fmt::Display>(err: E) -> EngineError {
    EngineError::runtime(format!("spec builder rejected the input: {err}"), None)
}

fn default_caps() -> HashSet<Capability> {
    [
        Capability::AuditWrite,
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fowner,
        Capability::Fsetid,
        Capability::Kill,
        Capability::Mknod,
        Capability::NetBindService,
        Capability::NetRaw,
        Capability::Setfcap,
        Capability::Setgid,
        Capability::Setpcap,
        Capability::Setuid,
        Capability::SysChroot,
    ]
    .into_iter()
    .collect()
}

fn standard_mounts() -> Result<Vec<oci_spec::runtime::Mount>, EngineError> {
    fn mount(
        destination: &str,
        typ: &str,
        source: &str,
        options: &[&str],
    ) -> Result<oci_spec::runtime::Mount, EngineError> {
        MountBuilder::default()
            .destination(destination)
            .typ(typ.to_string())
            .source(source)
            .options(options.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
            .build()
            .map_err(spec_build_error)
    }
    let mut mounts = vec![
        mount("/proc", "proc", "proc", &[])?,
        mount(
            "/dev",
            "tmpfs",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"],
        )?,
        mount(
            "/dev/pts",
            "devpts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
            ],
        )?,
        mount(
            "/dev/shm",
            "tmpfs",
            "shm",
            &["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
        )?,
        mount(
            "/dev/mqueue",
            "mqueue",
            "mqueue",
            &["nosuid", "noexec", "nodev"],
        )?,
        // Fresh sysfs, not an rbind of the host's /sys. An rbind exposes the
        // host's firmware/DMI/device topology and full cgroup hierarchy to the
        // (untrusted) RUN step; a fresh sysfs is namespaced to the container.
        // It is mountable here because we always unshare a new network
        // namespace (and a user namespace when rootless), so the mounting
        // namespace owns the netns this sysfs is scoped to.
        mount(
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"],
        )?,
    ];

    // Bind the host's DNS config so RUN steps can resolve names through the
    // NAT'd egress `umf-networking` wires into the container netns. This is a
    // read-only ephemeral mount — it is never captured into a layer. We prefer
    // systemd-resolved's real upstream list over `/etc/resolv.conf`, which on a
    // resolved host is just the `127.0.0.53` stub — loopback, and so the
    // container netns's *own* (empty) loopback, not the host's.
    if let Some(src) = host_resolv_conf() {
        // Type *must* be `"bind"` (not `"none"`): libcontainer only creates a
        // missing destination as a regular *file* when the mount type is
        // literally `"bind"` — otherwise it `mkdir`s the destination, and then
        // bind-mounting a file onto a directory fails with `ENOTDIR`. Base
        // images such as `debian:slim` ship no `/etc/resolv.conf`, so the
        // runtime does create it here.
        mounts.push(mount("/etc/resolv.conf", "bind", src, &["bind", "ro"])?);
    }

    Ok(mounts)
}

/// Best host `resolv.conf` to surface inside the container: systemd-resolved's
/// real-upstream file if present, else the system one. `None` if neither
/// exists (the build simply gets no DNS, as before).
fn host_resolv_conf() -> Option<&'static str> {
    ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
}

// ── Minimal image-config shape we read ──────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct ImageConfigDoc {
    #[serde(default)]
    config: ImageConfigSection,
    #[serde(default)]
    rootfs: RootfsSection,
}

#[derive(Debug, Default, Deserialize)]
struct ImageConfigSection {
    #[serde(default, rename = "Env")]
    env: Vec<String>,
    #[serde(default, rename = "Entrypoint")]
    entrypoint: Vec<String>,
    #[serde(default, rename = "Cmd")]
    cmd: Vec<String>,
    #[serde(default, rename = "WorkingDir")]
    working_dir: String,
}

#[derive(Debug, Default, Deserialize)]
struct RootfsSection {
    #[serde(default)]
    diff_ids: Vec<String>,
}

// ── uid/gid lookup ──────────────────────────────────────────────────────────

// The build user's uid/gid (and whether we hold real host privilege) now come
// from `rootless::context`, captured at process entry — `getuid()` reports 0
// once we are inside our own user namespace, so it can't be read here.

// ── rootless id mapping ───────────────────────────────────────────────

/// Build the rootless uid/gid mappings for the runtime spec (the library-consumer
/// path, where youki creates the user namespace and applies these via
/// `newuidmap`/`newgidmap`).
///
/// A two-entry map backed by the caller's `/etc/subuid` + `/etc/subgid`
/// allocation: container id 0 maps to the host user (size 1), and container ids
/// `1..1+count` map onto the delegated sub-id range, so a base image's
/// non-root-owned files and `RUN --user <nonzero>` resolve to real ids instead
/// of `nobody`. There is no single-id fallback: a missing delegation or helper
/// is a hard, actionable error (see [`crate::subid::resolve_ranges`]) rather than
/// a silent degrade to `nobody`-owned output.
///
/// On the default CLI path this is unused — `rootless::enter` already put the
/// multi-id map on umf's own namespace and youki runs rootful inside it, so the
/// runtime spec carries no id mappings at all.
fn rootless_id_mappings(
    host_uid: u32,
    host_gid: u32,
) -> Result<(Vec<LinuxIdMapping>, Vec<LinuxIdMapping>), EngineError> {
    let id_map = |container: u32, host: u32, size: u32| -> Result<LinuxIdMapping, EngineError> {
        LinuxIdMappingBuilder::default()
            .container_id(container)
            .host_id(host)
            .size(size)
            .build()
            .map_err(spec_build_error)
    };

    let (sub_uid, sub_gid) = crate::subid::resolve_ranges(host_uid, host_gid)?;
    let uid_mappings = vec![
        id_map(0, host_uid, 1)?,
        id_map(1, sub_uid.start, sub_uid.count)?,
    ];
    let gid_mappings = vec![
        id_map(0, host_gid, 1)?,
        id_map(1, sub_gid.start, sub_gid.count)?,
    ];
    Ok((uid_mappings, gid_mappings))
}

/// A conservative resource ceiling for a single RUN step.
///
/// Caps process count so a fork-bomb in untrusted build instructions can't
/// exhaust the host. We intentionally impose no fixed memory ceiling:
/// legitimate builds (linkers, compilers) are memory-hungry and a wrong guess
/// would break them; a configurable memory cap can follow.
fn build_run_resources() -> Result<LinuxResources, EngineError> {
    let pids = LinuxPidsBuilder::default()
        .limit(4096_i64)
        .build()
        .map_err(spec_build_error)?;
    LinuxResourcesBuilder::default()
        .pids(pids)
        .build()
        .map_err(spec_build_error)
}

#[cfg(test)]
mod tests;
