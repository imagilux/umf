//! Bootable-OS target — assemble a bootable build's staging tree and emit it as
//! a plain layered OCI image.
//!
//! [`build_vm`] resolves the boot chain (FROM kernel, plus any
//! `ADD --from=<image>` userland), builds the L1+L2 staging tree (userland
//! unpack, kernel install, RUN steps in micro-VMs, runtime config), generates
//! the initramfs when `ENTRYPOINT` selects an init system,
//! then emits a single-layer OCI image carrying the boot-manifest labels
//! (`org.imagilux.umf.*`) that fully describe the projection. The image is
//! `type=bootable`: no disk is produced here — `umf compile` projects one on
//! demand (GPT / ESP / UKI / squashfs), and the disk is never an artifact.
//!
//! The AST shape [`build_vm`] expects: the **final** stage's `FROM` resolves to
//! a kernel artifact (`org.imagilux.umf.type=kernel`) — that label is what
//! marks the build bootable. FROM is the kernel source — there is no `KERNEL`
//! directive. Any earlier stages are container producers, built separately by
//! [`crate::engine_build::build_container_stages`] and consumed here via
//! `ADD --from=<stage>`. The initramfs is generated implicitly for init-system
//! ENTRYPOINTs; a binary-path ENTRYPOINT (appliance) skips it.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use oci_client::manifest::ImageIndexEntry;
use thiserror::Error;
use tracing::{debug, info, warn};
use umf_core::architecture::Architecture;
use umf_core::ast::{AddSource, Ast, Directive, EntrypointInit, FromSource};
use umf_core::l0::L0Kind;
use umf_core::label;

use crate::initrd::{InitrdError, InitrdReport, generate_initramfs};
use crate::kernel::{KernelLayout, KernelLayoutError, detect_kernel_layout};
use crate::resolver::{
    AddProvenance, AddResolveError, FromKernelProvenance, FromKernelResolveError, resolve_add,
    resolve_from_kernel,
};
use crate::runtime_config::{RuntimeConfigError, RuntimeConfigReport, apply_runtime_config};
use crate::vm_runner::RunStepResult;
use umf_engine::bundle::{Bundle, BundleOptions, LayerStrategy};
use umf_engine::error::EngineError;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerCompression, LayerSource, emit_image};
use umf_oci::registry::{ImageLayout, RegistryClient};
use umf_oci::staging::{BuildStaging, StagingError};

mod run;
mod validate;

use run::run_all_run_directives;
use validate::{pick_entrypoint, pick_flavor, validate_ast_for_vm};

// ── Constants ───────────────────────────────────────────────────────────────

/// Mount tag the host advertises for the 9p staging-dir share to VM RUN
/// guests. Mirrored in the run-flavour initramfs's mount command.
pub(crate) const MOUNT_TAG_STAGING: &str = "umfstage";

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the bootable-image builder ([`build_vm`]).
#[derive(Debug, Error)]
pub enum BootableBuildError {
    /// AST has zero stages — degenerate input. The parser doesn't produce
    /// this, but a directly-constructed AST might.
    #[error("empty AST: no stages to build")]
    EmptyAst,

    /// Bootable builds require `FROM` to resolve to a kernel artifact —
    /// `FROM scratch` has no kernel source and is rejected.
    #[error(
        "bootable build requires FROM to resolve to a kernel artifact \
         (got `FROM scratch` — no kernel source)"
    )]
    VmRequiresKernelFromRef,

    /// A container-only directive (`CMD` / `VOLUME` / `STOPSIGNAL`, which map to
    /// OCI container-config fields) appeared in a bootable build, where it has
    /// no meaning.
    #[error(
        "`{directive}` is a container-only directive and is not valid in a bootable build \
         (it sets an OCI container-config field with no bootable meaning)"
    )]
    ContainerOnlyDirective {
        /// The offending directive keyword.
        directive: &'static str,
    },

    /// A cross-stage `ADD --from=<stage>` referenced a stage name the build
    /// never produced. In a multi-stage bootable recipe the producer must be an
    /// earlier (container) stage; a single-stage recipe has no prior stage to
    /// copy from at all.
    #[error(
        "ADD --from={stage}: no such prior stage \
         (a cross-stage copy must reference a stage declared before the final bootable stage)"
    )]
    AddFromUnknownStage {
        /// The stage name the directive asked for.
        stage: String,
    },

    /// Materialising a producer stage's rootfs for a cross-stage
    /// `ADD --from=<stage>` failed (the engine could not unpack its image).
    #[error("ADD --from producer rootfs: {0}")]
    Engine(#[from] EngineError),

    /// `ADD --from=<image>` resolution failed (registry and cache both missed).
    #[error("ADD --from resolution: {0}")]
    AddResolve(#[from] AddResolveError),

    /// A local `ADD <path> <dst>` source doesn't exist in the build context.
    #[error("ADD source not found: `{path}` (context: {context})")]
    AddSourceNotFound {
        /// The recipe source path, as written.
        path: String,
        /// The build-context directory it was resolved against.
        context: String,
    },

    /// An `ADD` source or destination tried to escape its containment root via
    /// a `..` component — rejected before any filesystem join (the bootable
    /// analogue of the container engine's `reject_traversal`).
    #[error("ADD {kind} path escapes its root: `{path}`")]
    AddPathTraversal {
        /// Which side of the ADD the offending path is (`source` / `destination`).
        kind: &'static str,
        /// The offending path.
        path: String,
    },

    /// Fetching an `ADD <url>` payload failed (DNS / TLS / HTTP / size cap).
    #[error("ADD <url> fetch ({url}): {detail}")]
    AddUrlFetch {
        /// The URL that failed to fetch.
        url: String,
        /// Underlying failure detail.
        detail: String,
    },

    /// An `ADD <url>` payload sniffed as a compressed archive format the
    /// bootable target can't extract — only tar / tar.gz, or a plain file.
    #[error(
        "ADD <url> ({url}): {format} archives are not supported \
         (provide a tar / tar.gz archive, or a plain file)"
    )]
    AddUrlArchiveUnsupported {
        /// The URL whose payload was an unsupported archive.
        url: String,
        /// The sniffed format name.
        format: String,
    },

    /// Extracting an `ADD <url>` tar / tar.gz into staging failed — the payload
    /// sniffed as an archive but wasn't a valid one.
    #[error("ADD <url> extract ({url}, {format}): {detail}")]
    AddUrlExtract {
        /// The URL whose payload failed extraction.
        url: String,
        /// The sniffed format name.
        format: String,
        /// Underlying extract failure detail.
        detail: String,
    },

    /// FROM-kernel resolution failed (registry and cache both missed, or the
    /// resolved artifact wasn't labelled as a kernel).
    #[error("FROM kernel resolution: {0}")]
    FromKernelResolve(#[from] FromKernelResolveError),

    /// FROM-kernel layers unpacked into staging, but no `boot/vmlinuz-*` or
    /// `lib/modules/<release>/` layout could be detected.
    #[error("kernel layout: {0}")]
    KernelLayout(#[from] KernelLayoutError),

    /// Staging error (tempdir creation, tar unpack, …).
    #[error("staging: {0}")]
    Staging(#[from] StagingError),

    /// Initramfs generation error.
    #[error("initrd: {0}")]
    Initrd(#[from] InitrdError),

    /// Runtime-config (the EXPOSE firewall) emission failed.
    #[error("runtime config: {0}")]
    RuntimeConfig(#[from] RuntimeConfigError),

    /// One of the AST's `RUN` directives failed to execute in the
    /// micro-VM.
    #[error("vm runner: {0}")]
    RunStep(#[from] crate::vm_runner::RunStepError),

    /// A VM `RUN` step exited with a non-zero status — the build
    /// shouldn't continue stacking layers on a failed step.
    #[error(
        "RUN exited with status {exit_code} (command: {command})\n\
         --- serial output ---\n{serial_output}\n--- end ---"
    )]
    RunStepFailed {
        /// The shell command that failed.
        command: String,
        /// Exit code the guest reported.
        exit_code: i32,
        /// Captured guest stdout/stderr — primary diagnostic.
        serial_output: String,
    },

    /// The AST had a `RUN` directive but `qemu_path` wasn't set in the
    /// builder options. The host-requirements preflight should normally
    /// catch this earlier.
    #[error(
        "RUN requires `qemu_path` in BootableBuildOptions (the CLI sets this from `umf doctor`)"
    )]
    RunWithoutQemu,

    /// OCI image emission (layer blobs, config, manifest, index) failed.
    #[error("oci image emission: {0}")]
    Oci(#[from] umf_oci::registry::RegistryError),

    /// I/O error from the staging / image layer.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

// ── Options + result ────────────────────────────────────────────────────────

/// Configuration for [`build_vm`].
///
/// All fields default to "no override"; pass `&BootableBuildOptions::default()` for
/// the conventional bootable build (every component resolved from the registry).
/// Disk geometry is no longer here — a disk is a `umf compile` projection.
#[derive(Debug, Clone)]
pub struct BootableBuildOptions {
    /// Explicit path to a userland tarball (gzipped or plain) on the host.
    /// Short-circuits the `ADD --from=<image>` resolver chain for the first
    /// such directive — primarily a test seam.
    pub rootfs_path_override: Option<PathBuf>,
    /// Registry reference to try when resolving the `ADD --from=<image>`
    /// userland, e.g. `"registry.example.com/library/alpine:3.21.0"`. `None`
    /// skips the registry step of the chain (cache still runs).
    pub rootfs_registry_ref: Option<String>,
    /// Explicit path to a kernel tarball (gzipped or plain). The tarball
    /// is expected to expose `boot/vmlinuz-<release>` and
    /// `lib/modules/<release>/`. Short-circuits the FROM-kernel resolver.
    /// Test seam — no CLI flag exposes this; sovereign builds use the
    /// upcoming registry-URL replacement mechanism instead.
    pub from_kernel_path_override: Option<PathBuf>,
    /// Registry reference to try when resolving the FROM kernel artifact,
    /// e.g. `"ghcr.io/imagilux/kernel-linux:7.0"`. `None` skips the registry
    /// step (cache + override still work).
    pub from_kernel_registry_ref: Option<String>,
    /// When `Some`, the unpacked staging directory is persisted at the given
    /// path after the build finishes (otherwise it's dropped with the
    /// tempdir). Useful for inspection.
    pub staging_keep_path: Option<PathBuf>,
    /// Path to `qemu-system-x86_64` (or the equivalent for the target
    /// architecture) for VM RUN steps. Required when the AST has at least
    /// one `RUN` directive. The host-requirements preflight fills this in.
    pub qemu_path: Option<PathBuf>,
    /// Whether `/dev/kvm` is accessible — picks between hardware
    /// acceleration and TCG software emulation for VM RUN steps.
    pub kvm_available: bool,
    /// CPU architecture the bootable image targets. Recorded in the image
    /// config (the OCI `architecture` field) and selects the QEMU binary
    /// name for VM RUN steps; the projector (`umf compile`) reads it back to
    /// pick the UEFI fallback filename and bootloader binary at projection
    /// time. Defaults to [`Architecture::host`].
    pub architecture: Architecture,
    /// Compression codec for the emitted image layer — the CLI's
    /// `--compression`. Gzip default; zstd is the OCI 1.1 media type.
    pub compression: LayerCompression,
    /// Build-context directory that a local `ADD <path> <dst>` resolves its
    /// source against (the same context the container engine uses). Defaults
    /// to the current directory; the CLI sets it from the resolved recipe's
    /// context. URL and OCI ADD sources ignore it.
    pub context: PathBuf,
    /// `--build-arg NAME=VALUE` values from the CLI. They override an `ARG`'s
    /// declared default during `${VAR}` / `$VAR` substitution on
    /// the kernel `FROM` reference, the `ADD` operands, and the micro-VM `RUN`
    /// commands. The bootable image carries no per-directive history, so there
    /// is no leak surface to guard here — the values just drive the build.
    pub build_args: BTreeMap<String, String>,
}

impl Default for BootableBuildOptions {
    fn default() -> Self {
        Self {
            rootfs_path_override: None,
            rootfs_registry_ref: None,
            from_kernel_path_override: None,
            from_kernel_registry_ref: None,
            staging_keep_path: None,
            qemu_path: None,
            kvm_available: false,
            architecture: Architecture::host(),
            compression: LayerCompression::Gzip,
            context: PathBuf::from("."),
            build_args: BTreeMap::new(),
        }
    }
}

/// Description of a successfully built bootable-OS image.
///
/// `build` emits a plain layered OCI image (`type=bootable`) carrying the
/// rootfs, the embedded kernel, the generated initramfs, and the boot-manifest
/// labels. There is no disk here — a disk is a *projection*, produced on demand
/// by `umf compile` and never an OCI artifact.
#[derive(Debug, Clone)]
pub struct BootableBuildOutput {
    /// The emitted image's `index.json` entry (digest + ref-name annotation) —
    /// immediately consumable by `push` or external tooling.
    pub image: ImageIndexEntry,
    /// Boot-packaging flavor recorded in the boot manifest (`systemd-boot` /
    /// `uki`), from the `LABEL org.imagilux.umf.flavor` or the default. The
    /// projector reads it back to install the bootloader (or assemble a UKI).
    pub flavor: String,
    /// Where each `ADD --from=<image>` userland came from, in directive order
    /// (empty when the build adds no external image).
    pub add_sources: Vec<AddProvenance>,
    /// Where the FROM kernel artifact came from. Always present on a
    /// successful bootable build — the kernel is mandatory.
    pub kernel_source: FromKernelProvenance,
    /// Kernel layout in staging — release identifier + on-disk paths for
    /// `vmlinuz` and the modules tree (consumers: initramfs generator, the
    /// boot manifest).
    pub kernel_layout: KernelLayout,
    /// Summary of the generated initramfs. `None` for an appliance (binary
    /// ENTRYPOINT) or `none`; populated when ENTRYPOINT selects an init
    /// system (`systemd` / `openrc`).
    pub initrd_report: Option<InitrdReport>,
    /// Summary of the runtime-config directives that wrote into staging
    /// (the EXPOSE firewall). `None` when no staging tree was created.
    pub runtime_config: Option<RuntimeConfigReport>,
    /// One report per `RUN` directive in the AST, in source order. Empty
    /// when no RUN steps were executed.
    pub run_step_reports: Vec<RunStepResult>,
    /// Filesystem path the unpacked rootfs staging tree was persisted at,
    /// when [`BootableBuildOptions::staging_keep_path`] was set. `None` means
    /// the staging was dropped at the end of the build (default behaviour).
    pub staging_path: Option<PathBuf>,
}

// ── Public entry ────────────────────────────────────────────────────────────

/// Build the extra kernel-cmdline fragment for an appliance build — a
/// binary `ENTRYPOINT` runs directly as PID 1, so the kernel needs
/// `init=<binary>` (with any args after `--`, which the kernel passes to
/// init). Returns `None` for init-system (`systemd`/`openrc`) and `none`
/// ENTRYPOINTs, where the default init / generated initramfs handles PID 1.
/// The leading space lets the caller append it to the `options` line
/// unconditionally (`None` → empty → cmdline unchanged).
fn appliance_init_cmdline(init: &EntrypointInit) -> Option<String> {
    let argv: Vec<String> = match init {
        // Shell form has no exec-array structure to split on — it is a single
        // binary path. Whitespace-splitting it would corrupt a path that
        // legitimately contains spaces (`/opt/my app/run`), so the whole
        // string is the binary (no args). Use the exec form for argv.
        EntrypointInit::Path(s) => vec![s.value.clone()],
        EntrypointInit::Exec(v) => v.iter().map(|s| s.value.clone()).collect(),
        EntrypointInit::Systemd | EntrypointInit::OpenRc | EntrypointInit::None => return None,
    };
    let (binary, args) = argv.split_first()?;
    // The kernel cmdline tokenizer honours double quotes, so a binary path or
    // arg containing whitespace must be quoted to survive tokenization
    // (`init="/opt/my app/run"`).
    let mut frag = format!(" init={}", quote_cmdline_token(binary));
    if !args.is_empty() {
        // Everything after `--` on the kernel cmdline is handed to init as
        // its argv.
        frag.push_str(" --");
        for arg in args {
            frag.push(' ');
            frag.push_str(&quote_cmdline_token(arg));
        }
    }
    Some(frag)
}

/// Quote a kernel-cmdline token in double quotes when it contains whitespace
/// (the kernel's cmdline tokenizer honours double quotes), escaping any inner
/// `"`. Whitespace-free tokens pass through unchanged.
fn quote_cmdline_token(token: &str) -> String {
    if token.bytes().any(|b| b.is_ascii_whitespace()) {
        format!("\"{}\"", token.replace('"', "\\\""))
    } else {
        token.to_string()
    }
}

/// Build the bootable target — emit a single-layer `type=bootable` OCI image at
/// `tag`, carrying the assembled rootfs (ROOTFS + embedded kernel + generated
/// initramfs) and the `org.imagilux.umf.*` boot-manifest labels that fully
/// describe the projection. No disk is written here — `umf compile` projects a
/// disk from the image on demand, and the disk is never an OCI artifact.
///
/// `layout` is the on-disk OCI image-layout cache (consulted by FROM-kernel /
/// ROOTFS resolution for already-pulled artifacts, the destination for any
/// registry pull triggered by resolution, and where the emitted image is
/// written). `registry` is optional — when present, resolution may pull a
/// missing artifact from a remote registry; when `None`, only the cache and
/// host fallbacks are consulted.
///
/// On success a [`BootableBuildOutput`] describes the emitted image (its
/// `index.json` entry, the resolved boot chain, and the build reports).
#[tracing::instrument(
    level = "info",
    name = "umf.build.bootable",
    skip(ast, layout, registry, options, produced),
    fields(reference = %tag)
)]
pub async fn build_vm(
    ast: &Ast,
    layout: &ImageLayout,
    registry: Option<&RegistryClient>,
    tag: &str,
    options: &BootableBuildOptions,
    produced: &BTreeMap<String, String>,
) -> Result<BootableBuildOutput, BootableBuildError> {
    let stage = validate_ast_for_vm(ast)?;
    // Build-global `ARG` scope (pre-`FROM` ARGs resolved against `--build-arg`),
    // for `${VAR}` / `$VAR` substitution. The kernel `FROM` ref is
    // substituted against this — like the container path's FROM — while ADD and
    // RUN extend it positionally with any in-stage `ARG`.
    let globals = crate::arg_subst::resolve_global_args(ast, &options.build_args);
    let (flavor, flavor_defaulted) = pick_flavor(stage);
    let flavor = flavor.to_string();
    let entrypoint_init = pick_entrypoint(stage)
        .cloned()
        .unwrap_or(EntrypointInit::Systemd);
    if flavor_defaulted {
        warn!(
            reference = %tag,
            "no `LABEL org.imagilux.umf.flavor` set; defaulting to `systemd-boot` (classic). \
             Set it to `systemd-boot` or `uki` to be explicit."
        );
    }

    // FROM is the kernel artifact for bootable builds; `validate_ast_for_vm`
    // has already rejected the `FROM scratch` case. A `${VAR}` in the ref is
    // substituted against the build globals, mirroring the container FROM, so a
    // placeholder that didn't resolve surfaces as a clear kernel-resolution
    // error rather than silently building the wrong kernel.
    let from_kernel_ref = match &stage.from.source {
        FromSource::Reference(s) => crate::arg_subst::subst_with(&globals, s.value.as_str()),
        FromSource::Scratch => unreachable!("validate_ast_for_vm rejects FROM scratch"),
    };

    info!(
        reference = %tag,
        from_kernel = %from_kernel_ref,
        flavor = %flavor,
        "building bootable-OS image",
    );

    // The flavor is recorded in the boot manifest (below); resolving and
    // installing the bootloader onto the ESP (or assembling a UKI) is the
    // projector's job (`umf compile`), not the builder's — `build` stays
    // pure-OCI.

    // Resolve FROM as a kernel artifact (label-checked at introspection time).
    let from_kernel = resolve_from_kernel(
        &from_kernel_ref,
        options.architecture,
        registry,
        layout,
        options.from_kernel_path_override.as_deref(),
        options.from_kernel_registry_ref.as_deref(),
    )
    .await?;
    let kernel_source = from_kernel.provenance.clone();

    // Build the union L1 + L2 staging:
    //   1. `ADD <oci-ref> /` userland layers (L1, optional), in directive
    //      order — the rootfs(es). A build with no such directive (e.g. an
    //      appliance) skips this.
    //   2. local / URL / cross-stage `ADD <src> <dst>` file additions onto the
    //      userland, in source order.
    //   3. FROM kernel layers (L2, mandatory — vmlinuz + modules).
    //
    // `ADD --from=<stage>` copies out of an earlier container stage's rootfs
    // (materialised from `produced`); in a single-stage build `produced` is
    // empty, so any `--from` there resolves to a clear "no such prior stage"
    // error. ADDs are applied before the RUN pass — they populate the tree the
    // micro-VM RUN steps observe (see `run::run_all_run_directives`).
    let mut staging = BuildStaging::new()?;
    let mut add_sources = Vec::new();
    // The test-seam override (no CLI flag) applies to the first `ADD <oci-ref>`.
    let mut override_path = options.rootfs_path_override.as_deref();
    // `${VAR}` substitution scope, seeded from the build globals and extended
    // positionally by any in-stage `ARG`. Every ADD operand is
    // substituted for the operation; the bootable image keeps no per-directive
    // history, so there is nothing to keep an original for. The containment
    // guards inside the `add_*_into_staging` helpers run on the substituted
    // values, so a `${VAR}` that expands to a `..` cannot smuggle a traversal.
    let mut arg_scope = globals.clone();
    for directive in &stage.directives {
        match directive {
            Directive::Arg(arg) => {
                crate::arg_subst::apply_arg_to_scope(&mut arg_scope, arg, &options.build_args);
                continue;
            }
            Directive::Add(add) => {
                let dst = crate::arg_subst::subst_with(&arg_scope, add.destination.value.as_str());
                match &add.source {
                    // The userland (rootfs): an OCI image resolved through the
                    // registry → cache chain and unpacked. Recorded in
                    // `add_sources` (the rootfs provenance the CLI reports).
                    AddSource::Oci(image_ref) => {
                        let resolved_ref =
                            crate::arg_subst::subst_with(&arg_scope, image_ref.value.as_str());
                        let art = resolve_add(
                            &resolved_ref,
                            options.architecture,
                            registry,
                            layout,
                            override_path.take(),
                            options.rootfs_registry_ref.as_deref(),
                        )
                        .await?;
                        for layer in &art.layers {
                            staging.unpack_tarball(layer)?;
                        }
                        add_sources.push(art.provenance.clone());
                    }
                    // A Path source is either a cross-stage copy
                    // (`ADD --from=<stage>` — out of a prior container stage's
                    // rootfs) or a bare local copy (`ADD <path> <dst>` — out of
                    // the build context). Mirrors the engine's `apply_add`: only
                    // a Path source carries `--from`. Either way it lands on the
                    // staging tree, not in `add_sources`.
                    AddSource::Path(src) => {
                        let src_sub = crate::arg_subst::subst_with(&arg_scope, src.value.as_str());
                        if let Some(from) = add.from.as_ref() {
                            add_from_stage_into_staging(
                                from.value.as_str(),
                                &src_sub,
                                &dst,
                                produced,
                                layout,
                                options.architecture,
                                &staging,
                            )?;
                        } else {
                            add_local_into_staging(&options.context, &src_sub, &dst, &staging)?;
                        }
                    }
                    // A remote payload, fetched then either extracted (tar /
                    // tar.gz) or placed as a plain file onto the staging tree.
                    AddSource::Url(url) => {
                        let url_sub = crate::arg_subst::subst_with(&arg_scope, url.value.as_str());
                        add_url_into_staging(&url_sub, &dst, &staging).await?;
                    }
                }
            }
            _ => continue,
        }
    }
    for layer in &from_kernel.layers {
        staging.unpack_tarball(layer)?;
    }
    let kernel_layout = detect_kernel_layout(&staging)?;

    // Runtime config (the EXPOSE firewall, keyed off ENTRYPOINT's init system)
    // writes into staging's /etc tree.
    let runtime_config_report = Some(apply_runtime_config(stage, &mut staging)?);

    // RUN steps: each spawns a micro-VM whose root is the staging tree (via
    // 9p), executes the command, captures the exit code, and continues. The
    // build globals seed the `${VAR}` scope the RUN walk extends positionally.
    let run_step_reports =
        run_all_run_directives(stage, &staging, &kernel_layout, options, &globals).await?;

    // Initramfs generation: triggered when ENTRYPOINT selects an init system
    // (systemd / openrc). Binary-path or `none` ENTRYPOINTs skip it. The
    // generated image is written into the staging tree (`/boot`) so it ships in
    // the OCI layer; `umf compile` copies it onto the ESP at projection time.
    let initrd_report = if entrypoint_init.is_init_system() {
        let (bytes, report) = generate_initramfs(&staging, &kernel_layout)?;
        let dst = staging.path().join("boot").join(&report.filename);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, &bytes)?;
        Some(report)
    } else {
        debug!(
            ?entrypoint_init,
            "initramfs: skipped (ENTRYPOINT is not an init system)"
        );
        None
    };

    // Appliance shape: a binary ENTRYPOINT becomes the kernel's `init=`, so it
    // runs as PID 1 directly (no init system, no initramfs). Empty for
    // init-system builds, leaving the cmdline unchanged.
    let extra_cmdline = appliance_init_cmdline(&entrypoint_init).unwrap_or_default();

    // Boot manifest: `org.imagilux.umf.*` labels that fully describe the
    // projection, so `umf compile` can shape a disk from the image alone — no
    // recipe, no second resolution. The disk itself is never an artifact.
    let mut labels = BTreeMap::new();
    labels.insert(
        label::ENTRYPOINT.to_string(),
        entrypoint_label(&entrypoint_init),
    );
    labels.insert(
        label::KERNEL_RELEASE.to_string(),
        kernel_layout.release.clone(),
    );
    labels.insert(
        label::KERNEL_VMLINUZ.to_string(),
        rootfs_path(&kernel_layout.vmlinuz, staging.path()),
    );
    let cmdline = extra_cmdline.trim();
    if !cmdline.is_empty() {
        labels.insert(label::KERNEL_CMDLINE.to_string(), cmdline.to_string());
    }
    if let Some(report) = &initrd_report {
        labels.insert(
            label::INITRAMFS.to_string(),
            format!("/boot/{}", report.filename),
        );
    }
    labels.insert(
        label::ROOTFS_FS.to_string(),
        umf_core::boot::ROOTFS_FSTYPE.to_string(),
    );
    labels.insert(label::FLAVOR.to_string(), flavor.clone());

    // Emit the bootable-OS image: the whole staging tree (rootfs + embedded
    // kernel + initramfs) as one layer, carrying the boot manifest. `type=
    // bootable` — projectable to a disk by `umf compile`, and a valid `FROM`.
    let layer = LayerSource::from_directory_with(staging.path(), options.compression)?;
    let config = ImageConfig {
        architecture: options.architecture.oci_arch_string().to_string(),
        os: "linux".to_string(),
        umf_type: L0Kind::Bootable,
        container: ContainerConfig {
            labels,
            ..ContainerConfig::default()
        },
        ..ImageConfig::default()
    };
    let image = emit_image(layout, &[layer], &config, tag)?;
    info!(reference = %tag, digest = %image.digest, "built bootable-OS image");

    let staging_path = match &options.staging_keep_path {
        Some(target) => Some(persist_staging(staging, target)?),
        None => None,
    };

    Ok(BootableBuildOutput {
        image,
        flavor,
        add_sources,
        kernel_source,
        kernel_layout,
        initrd_report,
        runtime_config: runtime_config_report,
        run_step_reports,
        staging_path,
    })
}

/// In-image absolute path of `abs`, computed relative to the rootfs `root`
/// (staging `/tmp/x/boot/vmlinuz-7.0` under root `/tmp/x` → `/boot/vmlinuz-7.0`).
/// Recorded in the boot manifest so the projector locates the kernel without a
/// second filesystem walk.
fn rootfs_path(abs: &Path, root: &Path) -> String {
    let rel = abs.strip_prefix(root).unwrap_or(abs);
    format!("/{}", rel.display())
}

/// Boot-manifest `entrypoint` value: the init-system name, `appliance` for a
/// binary ENTRYPOINT (runs as PID 1), or `none`.
fn entrypoint_label(init: &EntrypointInit) -> String {
    match init {
        EntrypointInit::Systemd => "systemd",
        EntrypointInit::OpenRc => "openrc",
        EntrypointInit::Path(_) | EntrypointInit::Exec(_) => "appliance",
        EntrypointInit::None => "none",
    }
    .to_string()
}

/// Persist a staging tree at the path the caller supplied: detach the
/// tempdir from auto-cleanup and rename (or copy) its content into place.
fn persist_staging(staging: BuildStaging, target: &Path) -> Result<PathBuf, BootableBuildError> {
    if target.exists() {
        std::fs::remove_dir_all(target)?;
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let detached = staging.into_path();
    // rename is fine within a filesystem; fall back to a recursive copy when
    // it crosses filesystem boundaries (e.g. tempdir on tmpfs, target on
    // ext4).
    if let Err(err) = std::fs::rename(&detached, target) {
        if err.raw_os_error() == Some(libc_exdev_compat()) {
            crate::fsutil::copy_dir_recursive(&detached, target)?;
            std::fs::remove_dir_all(&detached)?;
        } else {
            return Err(BootableBuildError::Io(err));
        }
    }
    Ok(target.to_path_buf())
}

const fn libc_exdev_compat() -> i32 {
    // `EXDEV` — cross-device rename. Linux uses 18, but this is also the value
    // on every other Unix we care about. Hard-coded so we don't pull in the
    // libc crate just for one constant.
    18
}

// ── Local / URL ADD into staging ─────────────────────────────────────────────

/// Reject an ADD source/destination that would escape its containment root via
/// a `..` component. Mirrors the container engine's `reject_traversal` and the
/// `umf-oci` / `umf-compile` containment guards: every ADD path is resolved
/// relative to a root (the build context or the staging tree), and a `..`
/// would let `Path::join` climb out of it.
fn reject_traversal(kind: &'static str, raw: &str) -> Result<(), BootableBuildError> {
    if Path::new(raw)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(BootableBuildError::AddPathTraversal {
            kind,
            path: raw.to_string(),
        });
    }
    Ok(())
}

/// Resolve a recipe destination against the staging root. A leading `/` is
/// stripped so an absolute recipe path lands relative to the rootfs (the
/// staging tree) rather than the host root. The caller has already rejected
/// `..` traversal.
fn staging_join(staging_root: &Path, dst: &str) -> PathBuf {
    staging_root.join(dst.trim_start_matches('/'))
}

/// Apply docker's trailing-slash + source-shape rule to a local ADD
/// destination: a trailing `/` (or a directory source) makes `dst` a
/// directory the source lands inside; otherwise `dst` names the file.
fn compute_add_destination(dst: &str, src: &Path) -> String {
    if dst.ends_with('/') {
        let leaf = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        format!("{dst}{leaf}")
    } else if src.is_dir() {
        format!("{dst}/")
    } else {
        dst.to_string()
    }
}

/// Classify an `ADD <url>` payload by magic number, reading only its leading
/// bytes (the `ustar` magic sits at offset 257, so 512 is always enough). The
/// body stays on disk — the fetch streamed it there, so a multi-gigabyte
/// source never sits in memory.
fn sniff_format(path: &Path) -> Result<umf_oci::format::Format, BootableBuildError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 512];
    let mut filled = 0;
    while filled < head.len() {
        let n = file.read(&mut head[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(umf_oci::format::detect(&head[..filled]))
}

/// Copy a local `ADD <src> <dst>` from the build context onto the staging
/// tree. `src` is resolved against `context` (leading `/` stripped — an
/// absolute recipe source must not read from the host root); `dst` is resolved
/// against the staging root. Directories copy recursively, files byte-for-byte.
fn add_local_into_staging(
    context: &Path,
    src: &str,
    dst: &str,
    staging: &BuildStaging,
) -> Result<(), BootableBuildError> {
    reject_traversal("source", src)?;
    reject_traversal("destination", dst)?;

    let src_abs = context.join(src.trim_start_matches('/'));
    if !src_abs.exists() {
        return Err(BootableBuildError::AddSourceNotFound {
            path: src.to_string(),
            context: context.display().to_string(),
        });
    }

    let dst_inside = compute_add_destination(dst, &src_abs);
    reject_traversal("destination", &dst_inside)?;
    let dst_abs = staging_join(staging.path(), &dst_inside);
    if let Some(parent) = dst_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if src_abs.is_dir() {
        crate::fsutil::copy_dir_recursive(&src_abs, &dst_abs)?;
    } else {
        std::fs::copy(&src_abs, &dst_abs)?;
    }
    Ok(())
}

/// Apply a cross-stage `ADD --from=<stage> <src> <dst>` onto the staging tree.
/// Materialises the producing stage's rootfs (its in-layout OCI image, built
/// earlier by [`crate::engine_build::build_container_stages`]) and copies the
/// requested path out of it. The bootable analogue of the container engine's
/// `apply_add_from_stage`: the same producer lookup + `Bundle::from_image`
/// materialisation, but the copy lands directly in staging rather than a
/// synthesised layer (the bootable image ships one layer).
fn add_from_stage_into_staging(
    from_stage: &str,
    src: &str,
    dst: &str,
    produced: &BTreeMap<String, String>,
    layout: &ImageLayout,
    architecture: Architecture,
    staging: &BuildStaging,
) -> Result<(), BootableBuildError> {
    reject_traversal("source", src)?;
    reject_traversal("destination", dst)?;

    let producer_ref =
        produced
            .get(from_stage)
            .ok_or_else(|| BootableBuildError::AddFromUnknownStage {
                stage: from_stage.to_string(),
            })?;

    // Materialise the producer's merged rootfs in a tempdir (whiteouts applied,
    // platform selected) so we can copy the requested path out of it. The
    // Bundle's TempDir cleans up when it drops at end of scope.
    let bundle_opts = BundleOptions::for_host("umf-build-xref", LayerStrategy::Merge, architecture);
    let producer_bundle = Bundle::from_image(layout, producer_ref, &bundle_opts)?;

    // Resolve `src` against the producer's rootfs (leading `/` stripped — an
    // absolute recipe source must not read from the host root).
    let src_abs = producer_bundle.rootfs().join(src.trim_start_matches('/'));
    if !src_abs.exists() {
        return Err(BootableBuildError::AddSourceNotFound {
            path: src.to_string(),
            context: format!("stage `{from_stage}` rootfs"),
        });
    }

    let dst_inside = compute_add_destination(dst, &src_abs);
    reject_traversal("destination", &dst_inside)?;
    let dst_abs = staging_join(staging.path(), &dst_inside);
    if let Some(parent) = dst_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if src_abs.is_dir() {
        crate::fsutil::copy_dir_recursive(&src_abs, &dst_abs)?;
    } else {
        std::fs::copy(&src_abs, &dst_abs)?;
    }
    Ok(())
}

/// Apply an `ADD <url> <dst>` onto the staging tree. Reuses the container
/// engine's streaming fetch (`fetch_url` — rustls, size-capped, on-disk so a
/// huge payload never sits in memory), then sniffs the payload by magic
/// number:
///
/// - **tar / tar.gz** — extracted into `dst` through the same staging machinery
///   the OCI userland unpack uses (gzip decode + path-traversal containment).
/// - **other compressed archives** (zstd / xz / bzip2 / squashfs) — rejected;
///   provide a tar / tar.gz or a plain file.
/// - **anything else** — a plain file at `dst`, with docker's trailing-slash
///   rule (a `dst/` takes the URL's leaf name).
async fn add_url_into_staging(
    url: &str,
    dst: &str,
    staging: &BuildStaging,
) -> Result<(), BootableBuildError> {
    use umf_oci::format::Format;

    reject_traversal("destination", dst)?;

    let fetched = crate::engine_build::fetch::fetch_url(url)
        .await
        .map_err(|e| BootableBuildError::AddUrlFetch {
            url: url.to_string(),
            detail: e.to_string(),
        })?;
    let format = sniff_format(fetched.file.path())?;

    match format {
        Format::Tar | Format::Gzip => {
            // Extract through a scratch staging (decode + traversal
            // containment), then copy its tree under dst — the same guarantees
            // the userland unpack relies on. A `.gz` that is not actually a
            // gzipped tar, or a corrupt archive, surfaces as a clear error.
            let mut scratch = BuildStaging::new()?;
            scratch.unpack_tarball(fetched.file.path()).map_err(|e| {
                BootableBuildError::AddUrlExtract {
                    url: url.to_string(),
                    format: format.as_str().to_string(),
                    detail: e.to_string(),
                }
            })?;
            let dst_dir = staging_join(staging.path(), dst);
            std::fs::create_dir_all(&dst_dir)?;
            crate::fsutil::copy_dir_recursive(scratch.path(), &dst_dir)?;
        }
        Format::Zstd | Format::Xz | Format::Bzip2 | Format::Squashfs => {
            return Err(BootableBuildError::AddUrlArchiveUnsupported {
                url: url.to_string(),
                format: format.as_str().to_string(),
            });
        }
        Format::Unknown => {
            let dst_inside = if dst.ends_with('/') {
                format!("{dst}{}", crate::engine_build::fetch::url_leaf(url))
            } else {
                dst.to_string()
            };
            reject_traversal("destination", &dst_inside)?;
            let dst_abs = staging_join(staging.path(), &dst_inside);
            if let Some(parent) = dst_abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(fetched.file.path(), &dst_abs)?;
        }
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
