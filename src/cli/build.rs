//! `umf build` — end-to-end build of a `.umf` source into either an OCI
//! container artifact or a bootable-OS OCI image. The target is inferred from
//! the source: a `FROM` that resolves to a `type=kernel` artifact is a bootable
//! build; otherwise it's a container. (`umf compile` later projects a bootable
//! image into a disk.)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use oci_client::Reference;
use thiserror::Error;
use tracing::info;
use umf_builder::bootable::{BootableBuildError, BootableBuildOptions, build_vm};
use umf_oci::image::LayerCompression;

use umf_builder::engine_build::{
    EngineBuildError, EngineBuildOptions, SecretInput, SecretSource, build as engine_build,
    build_container_stages,
};
use umf_builder::host_requirements::{
    MissingRuntimeError, compute_requirements, detect_all_for, verify_requirements_for,
};
use umf_builder::introspect::introspect;
use umf_builder::resolver::resolve_add;
use umf_core::architecture::Architecture;
use umf_core::ast::{Ast, FromSource};
use umf_oci::registry::auth::resolve_auth_for;
use umf_oci::registry::{ImageLayout, RegistryClient, RegistryError};

use crate::cli::MetricsFormat;
use crate::cli::util::{self, CredentialError};

/// Errors surfaced by `umf build`.
#[derive(Debug, Error)]
pub(crate) enum CliBuildError {
    #[error(transparent)]
    Recipe(#[from] util::RecipeResolveError),
    #[error("cannot read {path}: {err}")]
    ReadFile { path: String, err: std::io::Error },
    #[error("parse failed — see diagnostics above")]
    Parse,
    #[error("--tag must be a valid OCI reference: {0}")]
    InvalidTag(String),
    #[error("--secret spec {0:?}: {1}")]
    InvalidSecretSpec(String, String),
    #[error("--build-arg {0:?}: {1}")]
    InvalidBuildArg(String, String),
    #[error("cannot resolve layout directory: {0}")]
    LayoutDir(String),
    #[error("--tag is required for container builds (got `FROM {0}`)")]
    MissingContainerTag(String),
    #[error("--tag is required for bootable builds (got `FROM {0}`)")]
    MissingBootableTag(String),
    #[error("--secret is only meaningful for container builds")]
    SecretOnVm,
    #[error("--staging-keep is only meaningful for bootable builds")]
    StagingKeepOnContainer,
    #[error("--metrics-output is only meaningful for container builds")]
    MetricsOutputOnBootable,
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("failed to read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--platform {0:?}: {1}")]
    BadPlatform(String, String),
    #[error("{0}")]
    MissingRuntime(#[from] MissingRuntimeError),
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
    #[error("build: {0}")]
    EngineBuild(#[from] EngineBuildError),
    #[error("build (vm): {0}")]
    Bootable(#[from] BootableBuildError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl From<CredentialError> for CliBuildError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf build` flags. Threaded through `run_build` to avoid a
/// many-argument function signature.
pub(crate) struct BuildArgs<'a> {
    pub(crate) path: Option<&'a Path>,
    pub(crate) file: Option<&'a Path>,
    pub(crate) tag: Option<&'a str>,
    pub(crate) platform: Option<String>,
    pub(crate) compression: LayerCompression,
    pub(crate) secret_specs: &'a [String],
    pub(crate) build_arg_specs: &'a [String],
    pub(crate) push: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
    pub(crate) staging_keep: Option<&'a Path>,
    pub(crate) metrics: MetricsFormat,
    pub(crate) metrics_output: Option<&'a Path>,
}

/// Parse the source, run host preflight, then dispatch to the bootable or
/// container build path (a `FROM` resolving to a `type=kernel` artifact marks a
/// bootable recipe; see `probe_bootable`).
pub(crate) fn run_build(args: BuildArgs<'_>) -> Result<(), CliBuildError> {
    // Resolve the recipe: explicit -f/--file, an explicit recipe file,
    // or Containerfile/Dockerfile discovery inside a directory (default
    // the current directory). See `util::resolve_recipe`.
    let resolved = util::resolve_recipe(args.path, args.file)?;
    let source =
        std::fs::read_to_string(&resolved.recipe).map_err(|err| CliBuildError::ReadFile {
            path: resolved.recipe.display().to_string(),
            err,
        })?;
    let source_name = resolved.recipe.display().to_string();
    let ast = match umf_parser::parse_with_warnings(&source) {
        Ok((ast, warnings)) => {
            if !warnings.is_empty() {
                let mut stderr = std::io::stderr().lock();
                let _ = umf_parser::diagnostics::report_all(
                    &warnings,
                    &mut stderr,
                    &source_name,
                    &source,
                );
            }
            ast
        }
        Err(diagnostics) => {
            let mut stderr = std::io::stderr().lock();
            let _ = umf_parser::diagnostics::report_all(
                &diagnostics,
                &mut stderr,
                &source_name,
                &source,
            );
            return Err(CliBuildError::Parse);
        }
    };

    let preflight_architecture = match &args.platform {
        Some(p) => Architecture::from_platform_str(p)
            .map_err(|e| CliBuildError::BadPlatform(p.clone(), e.to_string()))?,
        None => Architecture::host(),
    };

    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliBuildError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;
    info!(layout = %layout_dir.display(), "layout ready");

    let rt = tokio::runtime::Runtime::new()?;

    // Target inference: a `FROM` resolving to a `type=kernel`
    // artifact is a bootable build; everything else is a container. This pulls
    // `FROM` into the layout — the same fetch the build needs anyway.
    let bootable = probe_bootable(&args, &ast, &layout, &rt);

    // Verify the host carries every runtime this build will exercise. Fast
    // feedback when a bootable build needs qemu. Detection uses the target
    // architecture (from --platform when supplied) so cross-arch builds look up
    // `qemu-system-aarch64` not `qemu-system-x86_64`.
    let required = compute_requirements(&ast, bootable);
    let detected = verify_requirements_for(&required, preflight_architecture)?;
    if required.qemu && !detected.kvm_status.is_accessible() {
        eprintln!(
            "warning: /dev/kvm is not accessible ({:?}); VM RUN steps will fall back to \
             slow TCG software emulation",
            detected.kvm_status,
        );
    }

    // Record the build in the process registry (visible via `umf ps`).
    // We register here — past parse + host preflight — so user errors
    // before any real work (missing file, parse failure) don't leave a
    // process record; a build that actually starts and then fails does.
    let recipe = resolved
        .recipe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    let guard = super::process::RunningGuard::start(
        super::process::ProcessKind::Build,
        args.tag
            .map(str::to_string)
            .unwrap_or_else(|| recipe.clone()),
        format!("build {recipe}"),
        args.tag.map(str::to_string),
        None,
    );
    let result = if bootable {
        run_vm_build(&args, &ast, &layout, &layout_dir, &resolved.context, &rt)
    } else {
        run_container_build(
            &args,
            &ast,
            &layout,
            &layout_dir,
            &resolved.context,
            preflight_architecture,
            &rt,
        )
    };
    match result {
        Ok(()) => {
            guard.exited(0);
            Ok(())
        }
        Err(e) => {
            guard.failed();
            Err(e)
        }
    }
}

/// Decide whether the recipe builds a bootable artifact: the **final** stage's
/// `FROM` resolves to a `type=kernel` artifact (`FROM` is the sole
/// shape signal; there is no `BOOTLOADER` / `FIRMWARE` directive). In a
/// multi-stage recipe the earlier stages are container producers; only the last
/// stage's `FROM` decides container-vs-bootable.
///
/// - `FROM scratch` is never bootable.
/// - Otherwise the `FROM` reference is pulled into the layout (best-effort,
///   warming the cache the build needs anyway) and its `org.imagilux.umf.type`
///   label is read. A resolution failure falls back to "not bootable" so the
///   container path surfaces a precise error rather than this probe.
fn probe_bootable(
    args: &BuildArgs<'_>,
    ast: &Ast,
    layout: &ImageLayout,
    rt: &tokio::runtime::Runtime,
) -> bool {
    let Some(stage) = ast.stages.last() else {
        return false;
    };
    let reference = match &stage.from.source {
        FromSource::Scratch => return false,
        FromSource::Reference(r) => r.value.as_str().to_string(),
    };
    // Build the registry client from the parsed ref (picks up --insecure-registry
    // for that host). An unparseable ref can't build anyway → not bootable.
    let Ok(parsed) = reference.parse::<Reference>() else {
        return false;
    };
    let client = util::registry_client_for(&parsed, args.insecure_registry);
    // The layout cache is keyed by the canonical ref (`Reference::whole()`, e.g.
    // `docker.io/imagilux/kernel-linux:7.0`), so introspect that form — not the
    // literal source string — or a cached image is missed.
    let canonical = parsed.whole();
    rt.block_on(async {
        // Pull (best-effort) so the type label is readable from the cache.
        let _ = resolve_add(
            &reference,
            Architecture::host(),
            Some(&client),
            layout,
            None,
            None,
        )
        .await;
        introspect(layout, &canonical)
            .map(|p| p.kind.is_kernel())
            .unwrap_or(false)
    })
}

/// Container-target build path: parse + engine build, optional push,
/// metrics summary.
fn run_container_build(
    args: &BuildArgs<'_>,
    ast: &Ast,
    layout: &ImageLayout,
    layout_dir: &Path,
    context_dir: &Path,
    architecture: Architecture,
    rt: &tokio::runtime::Runtime,
) -> Result<(), CliBuildError> {
    let tag = args.tag.ok_or_else(|| {
        // First-stage FROM is non-scratch — surface it for context.
        let from = ast
            .stages
            .first()
            .and_then(|s| match &s.from.source {
                FromSource::Reference(r) => Some(r.value.as_str().to_string()),
                FromSource::Scratch => None,
            })
            .unwrap_or_else(|| "<unknown>".to_string());
        CliBuildError::MissingContainerTag(from)
    })?;

    // `--staging-keep` persists the bootable staging tree; a container build
    // has no such tree and would silently ignore it. Reject rather than drop.
    if args.staging_keep.is_some() {
        return Err(CliBuildError::StagingKeepOnContainer);
    }

    let reference: Reference = tag
        .parse()
        .map_err(|e: oci_client::ParseError| CliBuildError::InvalidTag(e.to_string()))?;

    let secrets = args
        .secret_specs
        .iter()
        .map(|s| parse_secret_spec(s).map_err(|e| CliBuildError::InvalidSecretSpec(s.clone(), e)))
        .collect::<Result<Vec<_>, _>>()?;

    let build_args = parse_build_args(args.build_arg_specs)?;

    info!(engine = "umf", "container build");
    let engine_options = EngineBuildOptions {
        secrets,
        architecture,
        compression: args.compression,
        build_args,
        ..EngineBuildOptions::default()
    };

    // Begin the metrics collector. External timing only in this
    // first slice (`umf-builder::metrics::MetricsLayer` is the
    // tracing-layer-based phase-timing path; wiring it into the
    // already-installed subscriber is its own follow-up).
    let metrics_builder = umf_builder::metrics::BuildMetrics::start();

    let entry = rt.block_on(engine_build(layout, context_dir, ast, tag, &engine_options))?;
    info!(
        manifest = %entry.digest,
        size = entry.size,
        "image registered in layout",
    );
    println!("Built {tag} -> {digest}", digest = entry.digest);

    let pushed = if args.push {
        let client = util::registry_client_for(&reference, args.insecure_registry);
        info!(reference = %tag, "pushing to registry");
        let override_ = util::credential_override(args.username, args.password_stdin)?;
        let auth = resolve_auth_for(Some(reference.registry()), &override_);
        rt.block_on(client.push(&reference, tag, layout, &auth))?;
        println!("Pushed {tag}");
        true
    } else {
        println!(
            "Layout: {layout}\n(use --push to upload to the registry implied by --tag)",
            layout = layout_dir.display(),
        );
        false
    };

    // Read the just-emitted manifest to derive layer count + total
    // bytes for the metrics summary. Best-effort — a malformed
    // manifest here would already have failed the build above; this
    // is purely informational.
    let (layer_count, total_bytes) = read_layer_stats(layout, &entry.digest).unwrap_or((0, 0));
    // Pushed-byte total is the manifest's layer + config sizes — what crossed
    // the wire — so it equals `total_bytes` when we pushed, else `None`.
    let pushed_bytes = pushed.then_some(total_bytes);

    let metrics = metrics_builder.finish_with_image(layer_count, total_bytes, pushed_bytes);
    emit_metrics_report(&metrics, args.metrics, args.metrics_output);

    Ok(())
}

/// Read the on-disk manifest for `ref_digest` and return
/// `(layer_count, total_compressed_bytes)`. Used for the metrics
/// summary; returns `None` when the manifest can't be parsed (a
/// soft failure — the build itself already succeeded).
fn read_layer_stats(layout: &ImageLayout, ref_digest: &str) -> Option<(usize, i64)> {
    let bytes = layout.read_blob(ref_digest).ok()?;
    // The "ref entry digest" could point at either an image manifest
    // or an image index. For the metrics summary we just need the
    // top-level layer count + sum-of-sizes; for an index we approximate
    // by summing the per-child sizes (close enough for a "compressed
    // bytes" line).
    let parsed: oci_client::manifest::OciManifest = serde_json::from_slice(&bytes).ok()?;
    match parsed {
        oci_client::manifest::OciManifest::Image(m) => {
            let total: i64 = m.layers.iter().map(|l| l.size).sum();
            Some((m.layers.len(), total))
        }
        oci_client::manifest::OciManifest::ImageIndex(index) => {
            // Summed child manifest sizes; not the layer-bytes per se
            // but the closest "total payload" the index represents.
            let total: i64 = index.manifests.iter().map(|m| m.size).sum();
            Some((index.manifests.len(), total))
        }
    }
}

/// Format `metrics` per `format` and write it to stderr (and to
/// `output_path` when present). `MetricsFormat::None` suppresses
/// the report entirely.
fn emit_metrics_report(
    metrics: &umf_builder::metrics::BuildMetrics,
    format: MetricsFormat,
    output_path: Option<&Path>,
) {
    let rendered = match format {
        MetricsFormat::None => return,
        MetricsFormat::Text => metrics.render_text(),
        MetricsFormat::Json => match serde_json::to_string_pretty(metrics) {
            Ok(json) => format!("{json}\n"),
            Err(err) => {
                eprintln!("warning: failed to serialise metrics as JSON: {err}");
                return;
            }
        },
    };
    eprint!("{rendered}");
    if let Some(path) = output_path
        && let Err(err) = std::fs::write(path, &rendered)
    {
        eprintln!(
            "warning: failed to write metrics to {}: {err}",
            path.display(),
        );
    }
}

/// Bootable-target build path: resolve firmware / bootloader / rootfs /
/// kernel and emit a sparse raw disk image.
fn run_vm_build(
    args: &BuildArgs<'_>,
    ast: &Ast,
    layout: &ImageLayout,
    _layout_dir: &Path,
    context: &Path,
    rt: &tokio::runtime::Runtime,
) -> Result<(), CliBuildError> {
    let tag = args.tag.ok_or_else(|| {
        // The final stage's FROM is the bootable (kernel) source — surface it.
        let from = ast
            .stages
            .last()
            .and_then(|s| match &s.from.source {
                FromSource::Reference(r) => Some(r.value.as_str().to_string()),
                FromSource::Scratch => None,
            })
            .unwrap_or_else(|| "<unknown>".to_string());
        CliBuildError::MissingBootableTag(from)
    })?;
    let reference: Reference = tag
        .parse()
        .map_err(|e: oci_client::ParseError| CliBuildError::InvalidTag(e.to_string()))?;

    // Secrets are container-engine specific (the bootable RUN backend is qemu,
    // wired separately). `--push` is fine — a bootable image is a plain OCI
    // image like any other.
    if !args.secret_specs.is_empty() {
        return Err(CliBuildError::SecretOnVm);
    }
    // The bootable path emits no build-metrics report, so `--metrics-output`
    // would write nothing. Reject rather than silently no-op.
    if args.metrics_output.is_some() {
        return Err(CliBuildError::MetricsOutputOnBootable);
    }
    let architecture = match &args.platform {
        Some(p) => Architecture::from_platform_str(p)
            .map_err(|e| CliBuildError::BadPlatform(p.clone(), e.to_string()))?,
        None => Architecture::host(),
    };

    // `--build-arg NAME=VALUE` drives `${VAR}` substitution on the kernel FROM
    // ref, ADD operands, and micro-VM RUN commands. Parsed once and
    // shared with the earlier container stages below.
    let build_args = parse_build_args(args.build_arg_specs)?;

    let mut options = BootableBuildOptions {
        compression: args.compression,
        // Local `ADD <path> <dst>` resolves its source against the build
        // context, the same directory the container engine uses.
        context: context.to_path_buf(),
        build_args: build_args.clone(),
        ..BootableBuildOptions::default()
    };
    if let Some(p) = args.staging_keep {
        options.staging_keep_path = Some(p.to_path_buf());
    }
    options.architecture = architecture;
    let detected = detect_all_for(architecture);
    options.qemu_path = detected.qemu_path.clone();
    options.kvm_available = detected.kvm_status.is_accessible();

    // The FROM kernel + `ADD <oci-ref>` userland resolve through registry →
    // cache; construct a client. Resolution hits the cache first, so a
    // fully-cached / air-gapped build never touches the network.
    let registry = RegistryClient::new();

    // Multi-stage bootable: build the earlier stages as container images first
    // (via the engine), producing the stage-name → in-layout-ref map the final
    // bootable stage's `ADD --from=<stage>` reads from. A single-stage recipe
    // has no earlier stages → an empty map, and the engine isn't invoked at all
    // (the single-stage bootable path stays byte-identical).
    let produced = if ast.stages.len() > 1 {
        let engine_options = EngineBuildOptions {
            architecture,
            compression: args.compression,
            // The earlier (container) stages of a bootable build perform ARG
            // substitution like any container build; the final bootable stage
            // substitutes too (kernel FROM, ADD operands, micro-VM RUN).
            build_args: build_args.clone(),
            ..EngineBuildOptions::default()
        };
        info!(
            stages = ast.stages.len(),
            "building earlier container stages"
        );
        rt.block_on(build_container_stages(
            layout,
            context,
            ast,
            &engine_options,
        ))?
    } else {
        BTreeMap::new()
    };

    info!(reference = %tag, "building bootable-OS image");
    let out = rt.block_on(build_vm(
        ast,
        layout,
        Some(&registry),
        tag,
        &options,
        &produced,
    ))?;

    let rootfs_descr = if out.add_sources.is_empty() {
        "<none>".to_string()
    } else {
        out.add_sources
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let kernel_descr = format!(
        "{:?} (release {})",
        out.kernel_source, out.kernel_layout.release
    );
    let staging_descr = out
        .staging_path
        .as_ref()
        .map(|p| format!("{}", p.display()))
        .unwrap_or_else(|| "<dropped>".to_string());
    println!(
        "Built bootable image {tag} -> {digest}\n  \
         type: bootable (project to a disk with `umf compile`)\n  \
         flavor: {flavor}\n  \
         rootfs: {rsrc}\n  \
         kernel: {ksrc}\n  \
         staging: {staging}",
        digest = out.image.digest,
        flavor = out.flavor,
        rsrc = rootfs_descr,
        ksrc = kernel_descr,
        staging = staging_descr,
    );

    if args.push {
        let client = util::registry_client_for(&reference, args.insecure_registry);
        info!(reference = %tag, "pushing to registry");
        let override_ = util::credential_override(args.username, args.password_stdin)?;
        let auth = resolve_auth_for(Some(reference.registry()), &override_);
        rt.block_on(client.push(&reference, tag, layout, &auth))?;
        println!("Pushed {tag}");
    }

    Ok(())
}

/// Parse a `--secret id=<id>,src=<path>` / `id=<id>,env=<NAME>` spec
/// string. Mirrors BuildKit's CLI shape. Reused by `umf sign`, whose
/// Parse `--build-arg NAME=VALUE` specs into a name→value map.
///
/// Each spec must contain `=`; the name is everything before the first `=`
/// (non-empty), the value everything after (and may itself contain `=`). A
/// later spec for the same name wins.
pub(crate) fn parse_build_args(
    specs: &[String],
) -> Result<BTreeMap<String, String>, CliBuildError> {
    let mut out = BTreeMap::new();
    for spec in specs {
        let (name, value) = spec.split_once('=').ok_or_else(|| {
            CliBuildError::InvalidBuildArg(spec.clone(), "expected NAME=VALUE".to_string())
        })?;
        if name.is_empty() {
            return Err(CliBuildError::InvalidBuildArg(
                spec.clone(),
                "empty argument name".to_string(),
            ));
        }
        out.insert(name.to_string(), value.to_string());
    }
    Ok(out)
}

/// `--key` takes the same grammar.
pub(crate) fn parse_secret_spec(spec: &str) -> Result<SecretInput, String> {
    let mut id: Option<String> = None;
    let mut src: Option<PathBuf> = None;
    let mut env: Option<String> = None;
    for pair in spec.split(',') {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("missing `=` in `{pair}`"))?;
        match k {
            "id" => id = Some(v.to_string()),
            "src" => src = Some(PathBuf::from(v)),
            "env" => env = Some(v.to_string()),
            other => return Err(format!("unknown key {other:?} (expected id, src, env)")),
        }
    }
    let id = id.ok_or_else(|| "missing id=<id>".to_string())?;
    let source = match (src, env) {
        (Some(p), None) => SecretSource::File(p),
        (None, Some(name)) => SecretSource::Env { name },
        (Some(_), Some(_)) => return Err("specify exactly one of src= or env=".to_string()),
        (None, None) => return Err("specify src=<path> or env=<NAME>".to_string()),
    };
    Ok(SecretInput { id, source })
}

/// Read the material a [`SecretInput`] points at: the file contents for
/// `src=`, or the environment variable's value for `env=`. Used by `umf
/// sign` to load a signing key through the same channel as build secrets.
pub(crate) fn read_secret_material(input: &SecretInput) -> std::io::Result<Vec<u8>> {
    match &input.source {
        SecretSource::File(path) => std::fs::read(path),
        SecretSource::Env { name } => std::env::var(name).map(String::into_bytes).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("environment variable {name} is not set"),
            )
        }),
    }
}
