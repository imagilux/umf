//! Engine-backed container build.
//!
//! Walks an AST `Stage` (or sequence of stages) directly, lowering
//! each directive into `umf-engine` + `umf-oci` calls. Produces an
//! OCI image in `layout` under the requested ref — fully in-process,
//! no external container-build dependency.
//!
//! ## Per-stage pipeline
//!
//! 1. Pull the `FROM` image into the layout (skipped if already cached
//!    locally — see the private `base_image::pull_into_layout`).
//! 2. Read the base image's manifest + image-config; seed the
//!    in-progress [`ImageConfig`](umf_oci::image::ImageConfig) from it.
//! 3. Prepare a [`Bundle`] with the FROM rootfs unpacked.
//! 4. For each directive:
//!    - **RUN** — stacked overlay (lower = previous uppers + base
//!      rootfs, upper = fresh), execute via [`LibcontainerRuntime`],
//!      persist the upper-dir, package it as a [`LayerSource`](umf_oci::image::LayerSource).
//!    - **ADD** (local) — copy from `context_dir` into a synthesised
//!      upper-dir; package as a [`LayerSource`](umf_oci::image::LayerSource).
//!    - **ADD --from=&lt;stage&gt;** — materialise the producing stage's
//!      rootfs (via [`Bundle::from_image`](umf_engine::bundle::Bundle::from_image) against the stage's
//!      internal layout ref) and copy the requested path into a
//!      fresh upper-dir.
//!    - **WORKDIR / USER / ENV / ARG / SHELL / LABEL / ENTRYPOINT /
//!      CMD / VOLUME / STOPSIGNAL** — mutate `ImageConfig` in place, no layer.
//!    - **EXPOSE** — image-config `ExposedPorts`.
//! 5. Emit a final OCI image carrying the base image's layer chain,
//!    the newly-created layers (one per RUN/ADD), and the mutated
//!    image-config.
//!
//! ## Multi-stage orchestration
//!
//! Stages execute in AST order. Each non-final stage is emitted under
//! a deterministic internal ref (`umf-build/stage-N:internal`) so the
//! next stage's `ADD --from` can read its rootfs. Cross-stage refs
//! must point *backwards*: a forward reference, an unknown name, or a
//! self-reference produces an explicit pre-flight diagnostic
//! ([`EngineBuildError::AddFromForwardReference`] /
//! [`AddFromUnknownStage`](EngineBuildError::AddFromUnknownStage) /
//! [`AddFromSelf`](EngineBuildError::AddFromSelf)). The final stage
//! emits under the user-supplied `final_ref`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{debug, info};
use umf_core::architecture::Architecture;
use umf_core::ast::{Ast, Directive, FromSource};
use umf_engine::backends::LibcontainerRuntime;
use umf_engine::bundle::{Bundle, BundleOptions, LayerStrategy};
use umf_engine::error::EngineError;
use umf_oci::image::{LayerCompression, emit_image};
use umf_oci::registry::ImageLayout;
use umf_oci::registry::error::RegistryError;

use oci_client::manifest::ImageIndexEntry;
use tempfile::TempDir;

mod add;
mod base_image;
mod cache;
mod directives;
// `pub(crate)` so the bootable target can reuse the streaming URL fetch
// (`fetch_url`) + `url_leaf` for its own `ADD <url>` handling, rather than
// duplicating the rustls / size-cap / on-disk-streaming machinery.
pub(crate) mod fetch;
mod run;
mod secrets;
mod state;
mod util;

use base_image::{pull_into_layout, resolve_base_image, scratch_base_image};
use cache::StepCache;
use directives::{BuildCtx, StepFlow, apply_directive, classify_directive, describe_directive};
use secrets::{ResolvedSecrets, resolve_secrets};
use state::BuildState;

// Public re-exports — preserve the `umf_builder::engine_build::*` API
// surface after splitting definitions into submodules.
pub use secrets::{SecretInput, SecretSource};

// ── Public API ──────────────────────────────────────────────────────────────

/// Options the caller can override.
#[derive(Debug, Clone, Default)]
pub struct EngineBuildOptions {
    /// Target CPU architecture for the container build, from the CLI's
    /// `--platform` flag (Buildx convention). Defaults to the build host's
    /// architecture (via [`Architecture::default`]). Selects the manifest
    /// from a multi-arch base image index, is recorded as the emitted
    /// image's OCI `architecture`, and is folded into the RUN / ADD cache
    /// keys so a layout reused across arches can't cross-contaminate.
    pub architecture: Architecture,
    /// Compression codec for every layer this build packages — the CLI's
    /// `--compression`. Gzip (the [`Default`]) interoperates everywhere;
    /// zstd is the OCI 1.1 layer media type. Folded into the step-cache
    /// keys, so a layout shared across codecs can't cross-contaminate.
    pub compression: LayerCompression,
    /// libcontainer state-root override. `None` ⇒ a per-build tempdir,
    /// cleaned up automatically. Operators only set this for diagnostics
    /// (peeking at runtime state directly).
    pub state_root: Option<PathBuf>,
    /// Build-time secrets exposed to `RUN --mount=type=secret` steps by
    /// matching `id`. Resolved once per build; missing references
    /// produce a clear pre-flight error.
    pub secrets: Vec<SecretInput>,
    /// Optional [`umf_engine::BuildHook`] that the engine calls before
    /// and after each directive. `None` (the default) installs the
    /// [`umf_engine::NoopHook`]. The `umf debug build` CLI installs an
    /// interactive REPL hook; tests can install programmatic hooks.
    pub hook: Option<umf_engine::SharedHook>,
    /// `--build-arg NAME=VALUE` values from the CLI. They override an `ARG`'s
    /// declared default during `${VAR}` / `$VAR` substitution.
    /// The values drive the build and the layer-cache keys but never enter the
    /// image history or config.
    pub build_args: BTreeMap<String, String>,
}

/// Errors produced by the engine-backed builder.
#[derive(Debug, Error)]
pub enum EngineBuildError {
    /// [`build_single_stage`] was given a multi-stage AST. The
    /// wrapper is for callers that want to assert single-stage; use
    /// [`build`] for multi-stage recipes.
    #[error("`build_single_stage` got a multi-stage AST — call `build` for multi-stage recipes")]
    MultiStageNotSupported,

    /// Fetching an `ADD <url>` source failed (connection, HTTP status,
    /// or the payload exceeded its size ceiling).
    #[error("ADD {url}: fetch failed — {detail}")]
    AddUrlFetchFailed {
        /// The URL the directive asked for.
        url: String,
        /// Human-readable failure detail (never response content).
        detail: String,
    },

    /// An `ADD <url>` payload sniffed as an archive format the engine
    /// cannot extract yet (xz / bzip2 / zstd / zip). tar and tar.gz
    /// extract natively; anything unrecognised is placed as a plain file.
    #[error(
        "ADD {url}: fetched payload is a {format} archive, which is not extracted yet — \
        pre-extract it, or repackage as tar/tar.gz"
    )]
    AddUrlArchiveUnsupported {
        /// The URL the directive asked for.
        url: String,
        /// The sniffed format name.
        format: String,
    },

    /// An `ADD <url>` payload sniffed as tar / gzip but could not be
    /// extracted as a (optionally gzipped) tar: a lone `.gz` of a single
    /// file, or a corrupt archive. The gzip magic marks the payload as a
    /// compressed archive (the spec fingerprints by magic number, never by
    /// extension), so it is extracted rather than placed as a file.
    #[error(
        "ADD {url}: the fetched {format} payload could not be extracted as a tar archive: \
        {detail}. Use a plain-file source, or repackage the content as tar / tar.gz."
    )]
    AddUrlExtractFailed {
        /// The URL the directive asked for.
        url: String,
        /// The sniffed format name (`tar` or `gzip`).
        format: String,
        /// The underlying extraction failure.
        detail: String,
    },

    /// Tar staging of a fetched `ADD <url>` archive failed.
    #[error(transparent)]
    Staging(#[from] umf_oci::staging::StagingError),

    /// `ADD --from=<name>` references a stage name that hasn't been
    /// declared anywhere in the AST.
    #[error("ADD --from={stage} references an undefined stage")]
    AddFromUnknownStage {
        /// The stage name the directive asked for.
        stage: String,
    },

    /// `ADD --from=<name>` references a stage that's declared *later*
    /// in the AST. UMF requires backward references only; a forward
    /// ref would either be a typo or a cycle.
    #[error(
        "ADD --from={stage} references a stage declared later in the recipe — \
        cross-stage references must point at a prior stage"
    )]
    AddFromForwardReference {
        /// The stage name the directive asked for.
        stage: String,
    },

    /// `ADD --from=<name>` where `<name>` is the consumer stage's own
    /// name. Trivial cycle.
    #[error("ADD --from={stage} references the stage itself — that's a trivial cycle")]
    AddFromSelf {
        /// The stage's own name.
        stage: String,
    },

    /// A non-final stage of a multi-stage **bootable** recipe resolved its
    /// `FROM` to a kernel artifact. Only the final stage may be bootable; an
    /// earlier bootable stage feeding the final one (nested-bootable) is not
    /// supported. Build it as a separate image and consume it via `FROM`.
    #[error(
        "stage {stage_index} (`FROM {from}`) resolves to a kernel artifact, but only the final \
        stage of a bootable recipe may be bootable — earlier stages must be container-shaped \
        (FROM a base image or `scratch`)"
    )]
    NestedBootableStage {
        /// Zero-based index of the offending non-final stage.
        stage_index: usize,
        /// The `FROM` reference that resolved to a kernel.
        from: String,
    },

    /// `RUN --mount=type=secret,id=<id>` referenced an id that wasn't
    /// supplied to the build via `--secret`.
    #[error("RUN --mount=type=secret references id `{0}` but no matching --secret was supplied")]
    MissingSecret(String),

    /// Failed to read a secret source — file unreadable or env var
    /// unset.
    #[error("secret `{id}`: {reason}")]
    SecretResolution {
        /// Which secret id failed to resolve.
        id: String,
        /// Human-readable reason.
        reason: String,
    },

    /// ADD source not found on disk relative to the build context.
    #[error("ADD source `{path}` not found relative to the build context `{context}`")]
    AddSourceNotFound {
        /// What the recipe asked for.
        path: String,
        /// Build-context directory we resolved against.
        context: String,
    },

    /// `COPY` was given a remote source (a URL or an OCI image reference).
    /// `COPY` is the Docker-compatible plain copy of local-context files and
    /// `--from=<stage>` paths; fetching remote blobs or pulling OCI images is
    /// `ADD`'s job. Use `ADD` for those.
    #[error(
        "COPY source `{reference}` is {kind}; COPY copies local files and `--from=<stage>` paths only — use ADD for URLs / OCI images"
    )]
    CopyRemoteSource {
        /// The offending source string. (Named `reference`, not `source`, so
        /// `thiserror` does not mistake it for a `#[source]` error cause.)
        reference: String,
        /// What it resolved to: `"a URL"` or `"an OCI image reference"`.
        kind: &'static str,
    },

    /// An ADD source or destination path contains a `..` component that would
    /// escape its containment root (the build context, a producing stage's
    /// rootfs, or the consumer's upper-dir). Rejected to prevent arbitrary
    /// host-file reads into a layer and out-of-tree writes.
    #[error("ADD {kind} `{path}` escapes its containment root: `..` components are not allowed")]
    AddPathTraversal {
        /// Which side of the ADD was offending: `"source"` or `"destination"`.
        kind: &'static str,
        /// The offending path exactly as written in the recipe.
        path: String,
    },

    /// A RUN step exited non-zero. Carries the exit code and a short
    /// excerpt of the failing command for the diagnostic.
    #[error("RUN step `{summary}` exited with status {code}")]
    RunFailed {
        /// Exit code (`-1` ⇒ killed by signal).
        code: i32,
        /// Short rendering of the command, for the error message.
        summary: String,
    },

    /// The `FROM` base resolved to a multi-arch image index that carries
    /// no manifest for the requested `--platform` architecture. We refuse
    /// to fall back to an arbitrary arch: a `linux/arm64` build must not
    /// silently use the amd64 manifest.
    #[error(
        "no manifest for platform linux/{arch} in the base image index \
        (the FROM image does not publish that architecture)"
    )]
    NoManifestForPlatform {
        /// OCI-shorthand arch that was requested (`amd64` / `arm64`).
        arch: String,
    },

    /// The base image's manifest references more layers than its
    /// image-config has `diff_ids` for — corrupt or malformed.
    #[error("base image manifest / config disagree: {layers} layers vs {diff_ids} diff_ids")]
    BaseImageLayerCountMismatch {
        /// Layer descriptors on the manifest.
        layers: usize,
        /// `rootfs.diff_ids` entries on the image-config.
        diff_ids: usize,
    },

    /// Wraps a `umf_engine` runtime error.
    #[error(transparent)]
    Engine(#[from] EngineError),

    /// Wraps a `umf_oci` registry / layout error.
    #[error(transparent)]
    Registry(#[from] RegistryError),

    /// Reference parse failure (e.g. `--tag`).
    #[error("invalid OCI reference `{0}`: {1}")]
    InvalidReference(String, String),

    /// Filesystem error during ADD / bundle / layer staging.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode/decode of an OCI document.
    #[error("OCI document: {0}")]
    Json(#[from] serde_json::Error),
}

/// Build the recipe and emit the result under `final_ref` in `layout`.
///
/// Walks `ast.stages` in order. Each non-final stage emits an internal
/// OCI image at `umf-build/stage-N:internal`; the final stage emits
/// under `final_ref`. Cross-stage `ADD --from=<name>` reads from the
/// producer stage's rootfs (which must be a previously-declared stage).
///
/// `context_dir` is the recipe file's parent — local-source `ADD`
/// destinations resolve against it. `options.secrets` are materialised
/// once up front and bind-mounted into RUN steps that reference them
/// via `RUN --mount=type=secret,id=<id>`.
///
/// # Errors
/// See [`EngineBuildError`]. Common cases:
/// - [`EngineBuildError::AddFromUnknownStage`] / [`EngineBuildError::AddFromForwardReference`] / [`EngineBuildError::AddFromSelf`]
///   — invalid cross-stage `ADD --from`.
/// - [`EngineBuildError::MissingSecret`] — RUN referenced a secret id not supplied via `options.secrets`.
/// - [`EngineBuildError::RunFailed`] — a RUN step exited non-zero.
// `debug`, not `info`: at the default level this span's long inline context
// (`umf.engine.build{...}:`) prefixes every build event with fields the events
// already report. The span grouping surfaces at `--trace-level debug` / the
// json / pretty formats, mirroring the registry-client spans.
#[tracing::instrument(
        level = "debug",
        name = "umf.engine.build",
    skip(layout, context_dir, ast, options),
    fields(
        final_ref = %final_ref,
        stages = ast.stages.len(),
        context = %context_dir.display(),
    )
)]
pub async fn build(
    layout: &ImageLayout,
    context_dir: &Path,
    ast: &Ast,
    final_ref: &str,
    options: &EngineBuildOptions,
) -> Result<ImageIndexEntry, EngineBuildError> {
    if ast.stages.is_empty() {
        return Err(EngineBuildError::Engine(EngineError::runtime(
            "AST has no stages",
            None,
        )));
    }

    // Pre-flight cross-stage `ADD --from` references, then build the shared
    // runtime / secrets / cache / fetched-URL state the stage loop needs.
    // `build` runs every stage, so every stage's `ADD <url>` is prefetched.
    let setup = prepare_staged_build(layout, ast, &ast.stages, options).await?;

    // Walk stages in AST order. Each non-final stage emits an internal
    // OCI image into the layout under a deterministic temp ref so the
    // next stage's `ADD --from` can read its rootfs back. The last
    // stage emits under the user-supplied `final_ref`.
    let mut produced: BTreeMap<String, String> = BTreeMap::new();
    let mut last_entry: Option<ImageIndexEntry> = None;
    let globals = crate::arg_subst::resolve_global_args(ast, &options.build_args);

    for (i, stage) in ast.stages.iter().enumerate() {
        let is_last = i + 1 == ast.stages.len();
        let stage_ref = if is_last {
            final_ref.to_string()
        } else {
            format!("umf-build/stage-{i}:internal")
        };
        // Scope `ctx` so its immutable borrow of `produced` ends before the
        // `produced.insert` below.
        let entry = {
            let ctx = BuildCtx {
                runtime: &setup.runtime,
                layout,
                cache: &setup.cache,
                secrets: &setup.secrets,
                context_dir,
                produced: &produced,
                architecture: options.architecture,
                compression: options.compression,
                fetched_urls: &setup.fetched_urls,
                globals: &globals,
                build_args: &options.build_args,
            };
            build_one_stage(
                &ctx,
                stage,
                &stage_ref,
                options.hook.as_ref(),
                i + 1,
                ast.stages.len(),
            )
            .await?
        };
        if let Some(name) = stage.name.as_ref() {
            produced.insert(name.value.as_str().to_string(), stage_ref.clone());
        }
        last_entry = Some(entry);
    }

    last_entry
        .ok_or_else(|| EngineBuildError::Engine(EngineError::runtime("no stages built", None)))
}

/// Pre-flight every cross-stage `ADD --from=<name>` in the AST: each must
/// reference a *previously declared* stage. Walking stages in AST order, a
/// forward reference is a backward-cycle in disguise (the AST is malformed)
/// and a self-reference is a trivial cycle. Surfacing the diagnostic eagerly
/// means the caller never wastes execution building producer stages first.
fn check_cross_stage_refs(ast: &Ast) -> Result<(), EngineBuildError> {
    let names_in_order: Vec<Option<String>> = ast
        .stages
        .iter()
        .map(|s| s.name.as_ref().map(|n| n.value.as_str().to_string()))
        .collect();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (stage_idx, stage) in ast.stages.iter().enumerate() {
        for directive in &stage.directives {
            if let Directive::Add(add) = directive
                && let Some(from) = add.from.as_ref()
            {
                let target = from.value.as_str();
                if !seen_names.contains(target) {
                    let exists_later = names_in_order[stage_idx + 1..]
                        .iter()
                        .any(|n| n.as_deref() == Some(target));
                    let exists_self = names_in_order[stage_idx]
                        .as_deref()
                        .is_some_and(|n| n == target);
                    return Err(if exists_self {
                        EngineBuildError::AddFromSelf {
                            stage: target.to_string(),
                        }
                    } else if exists_later {
                        EngineBuildError::AddFromForwardReference {
                            stage: target.to_string(),
                        }
                    } else {
                        EngineBuildError::AddFromUnknownStage {
                            stage: target.to_string(),
                        }
                    });
                }
            }
        }
        if let Some(name) = &names_in_order[stage_idx] {
            seen_names.insert(name.clone());
        }
    }
    Ok(())
}

/// Shared pre-loop state for the staged build: the libcontainer runtime (plus
/// its auto-cleaned state tempdir), resolved build secrets, the step cache, and
/// every `ADD <url>` payload fetched up front. Both [`build`] (all stages) and
/// [`build_container_stages`] (the bootable orchestrator's earlier container
/// stages) consume it.
struct StagedBuildSetup {
    /// Held only for its `Drop` — auto-removes the libcontainer state
    /// directory. `None` when the caller pinned `options.state_root`.
    _state_tempdir: Option<TempDir>,
    runtime: LibcontainerRuntime,
    secrets: ResolvedSecrets,
    cache: StepCache,
    fetched_urls: BTreeMap<String, fetch::FetchedUrl>,
}

/// Run the cross-stage `ADD --from` pre-flight over the whole AST, then build
/// the shared runtime / secrets / cache / fetched-URL state the stage loop
/// needs. `fetch_stages` scopes the up-front `ADD <url>` fetch to the stages
/// this build actually walks — [`build`] passes every stage;
/// [`build_container_stages`] passes only the non-final container stages, so a
/// URL appearing solely in the final bootable stage isn't fetched here (the
/// bootable path fetches its own).
async fn prepare_staged_build(
    layout: &ImageLayout,
    ast: &Ast,
    fetch_stages: &[umf_core::ast::Stage],
    options: &EngineBuildOptions,
) -> Result<StagedBuildSetup, EngineBuildError> {
    check_cross_stage_refs(ast)?;

    // Shared libcontainer state directory across stages.
    let state_tempdir = if options.state_root.is_none() {
        Some(TempDir::new()?)
    } else {
        None
    };
    let state_root_path: PathBuf = match &options.state_root {
        Some(p) => p.clone(),
        None => state_tempdir
            .as_ref()
            .map(|td| td.path().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/run/umf-engine")),
    };
    let runtime = LibcontainerRuntime::new(&state_root_path)?;

    // Materialise build-time secrets once, up front. Each
    // `RUN --mount=type=secret,id=<id>` step looks the id up in this bag at
    // execution time. Drop guards in `ResolvedSecrets` clean up env-sourced
    // tempfiles when the returned struct is dropped.
    let secrets = resolve_secrets(&options.secrets)?;

    let cache = StepCache::for_layout(layout);

    // Fetch every `ADD <url>` payload once, up front, while we're async — the
    // directive walk is synchronous and reads the staged tempfiles from this
    // map. Re-fetched every build (docker semantics); the layer cache keys on
    // the payload digest, so unchanged content still hits.
    //
    // The URL is `${VAR}`-substituted against the stage's `ARG` scope as it
    // stands at that directive. Each stage's scope is seeded from
    // the build globals and extended positionally by in-stage `ARG`, mirroring
    // the real directive walk — so `apply_add_url` substitutes to the same
    // string and looks the payload up under the key stored here.
    let globals = crate::arg_subst::resolve_global_args(ast, &options.build_args);
    let mut fetched_urls: BTreeMap<String, fetch::FetchedUrl> = BTreeMap::new();
    for stage in fetch_stages {
        let mut stage_args = globals.clone();
        for directive in &stage.directives {
            if let Directive::Arg(arg) = directive {
                crate::arg_subst::apply_arg_to_scope(&mut stage_args, arg, &options.build_args);
            } else if let Directive::Add(add) = directive
                // Skip `COPY` (plain_copy): it has no business fetching a URL,
                // and `apply_add` rejects the remote source with a clear error.
                // Doing it here too keeps the prefetch from 404ing on a source
                // the build is going to refuse anyway.
                && !add.plain_copy
                && let umf_core::ast::AddSource::Url(spanned) = &add.source
            {
                let url = crate::arg_subst::subst_with(&stage_args, spanned.value.as_str());
                // Dedup the up-front fetch by resolved URL. The value is a
                // fallible async fetch, which doesn't fit `entry().or_insert_with`
                // (no async/`?` there), so the explicit contains/insert stays.
                #[allow(clippy::map_entry)]
                if !fetched_urls.contains_key(&url) {
                    let fetched = fetch::fetch_url(&url).await?;
                    fetched_urls.insert(url, fetched);
                }
            }
        }
    }

    Ok(StagedBuildSetup {
        _state_tempdir: state_tempdir,
        runtime,
        secrets,
        cache,
        fetched_urls,
    })
}

/// Build the non-final stages of a multi-stage **bootable** recipe as container
/// images, returning the `produced` map (stage-name → in-layout ref) the final
/// bootable stage's `ADD --from=<stage>` reads from.
///
/// Earlier stages are ordinary container builds — the same engine path the
/// all-container [`build`] uses, emitting internal images at
/// `umf-build/stage-N:internal` so a later stage materialises a producer's
/// rootfs by name. Only the *final* stage of a bootable recipe is bootable;
/// every earlier stage must be container-shaped (FROM a base image or
/// `scratch`). A non-final `FROM` that resolves to a kernel artifact is a
/// nested-bootable build and is rejected
/// ([`EngineBuildError::NestedBootableStage`]).
///
/// A single-stage AST has no earlier stages: this returns an empty map and does
/// no work, so the single-stage bootable path stays byte-identical.
pub async fn build_container_stages(
    layout: &ImageLayout,
    context_dir: &Path,
    ast: &Ast,
    options: &EngineBuildOptions,
) -> Result<BTreeMap<String, String>, EngineBuildError> {
    let stage_total = ast.stages.len();
    if stage_total <= 1 {
        return Ok(BTreeMap::new());
    }
    // Every stage but the last; the final (bootable) stage is built separately.
    let container_stages = &ast.stages[..stage_total - 1];

    // Nested-bootable guard: a non-final stage whose `FROM` resolves to a
    // kernel artifact would be a bootable stage feeding the final one. Pull +
    // introspect each (the pull warms the cache the build needs anyway; a
    // best-effort introspect only rejects on a *confirmed* kernel).
    for (i, stage) in container_stages.iter().enumerate() {
        if let FromSource::Reference(spanned) = &stage.from.source {
            let canonical = pull_into_layout(layout, spanned.value.as_str()).await?;
            if crate::introspect::introspect(layout, &canonical)
                .map(|p| p.kind.is_kernel())
                .unwrap_or(false)
            {
                return Err(EngineBuildError::NestedBootableStage {
                    stage_index: i,
                    from: spanned.value.as_str().to_string(),
                });
            }
        }
    }

    let setup = prepare_staged_build(layout, ast, container_stages, options).await?;

    let mut produced: BTreeMap<String, String> = BTreeMap::new();
    let globals = crate::arg_subst::resolve_global_args(ast, &options.build_args);
    for (i, stage) in container_stages.iter().enumerate() {
        let stage_ref = format!("umf-build/stage-{i}:internal");
        // Scope `ctx` so its immutable borrow of `produced` ends before the
        // `produced.insert` below. The emitted entry isn't needed here — only
        // the internal ref recorded in `produced` matters to a later stage.
        {
            let ctx = BuildCtx {
                runtime: &setup.runtime,
                layout,
                cache: &setup.cache,
                secrets: &setup.secrets,
                context_dir,
                produced: &produced,
                architecture: options.architecture,
                compression: options.compression,
                fetched_urls: &setup.fetched_urls,
                globals: &globals,
                build_args: &options.build_args,
            };
            let _entry = build_one_stage(
                &ctx,
                stage,
                &stage_ref,
                options.hook.as_ref(),
                i + 1,
                stage_total,
            )
            .await?;
        }
        if let Some(name) = stage.name.as_ref() {
            produced.insert(name.value.as_str().to_string(), stage_ref.clone());
        }
    }
    Ok(produced)
}

/// Single-stage convenience wrapper kept for backwards compatibility.
/// Equivalent to [`build`] but explicitly errors if the AST has more
/// than one stage — useful for callers that want to assert the
/// recipe is single-stage before invoking the engine.
///
/// # Errors
/// [`EngineBuildError::MultiStageNotSupported`] if the AST has more than
/// one stage; otherwise propagates the error from [`build`].
pub async fn build_single_stage(
    layout: &ImageLayout,
    context_dir: &Path,
    ast: &Ast,
    final_ref: &str,
    options: &EngineBuildOptions,
) -> Result<ImageIndexEntry, EngineBuildError> {
    if ast.stages.len() > 1 {
        return Err(EngineBuildError::MultiStageNotSupported);
    }
    build(layout, context_dir, ast, final_ref, options).await
}

// `debug` for the same reason as the parent `umf.engine.build` span:
// keep the default info build log free of a long span prefix.
#[tracing::instrument(
        level = "debug",
        name = "umf.engine.build.stage",
    skip(ctx, stage, hook),
    fields(stage_ref = %stage_ref, directives = stage.directives.len())
)]
async fn build_one_stage(
    ctx: &BuildCtx<'_>,
    stage: &umf_core::ast::Stage,
    stage_ref: &str,
    hook: Option<&umf_engine::SharedHook>,
    stage_index: usize,
    stage_total: usize,
) -> Result<ImageIndexEntry, EngineBuildError> {
    let layout = ctx.layout;
    let scratch = matches!(stage.from.source, FromSource::Scratch);
    let from_ref_str = match &stage.from.source {
        FromSource::Scratch => "scratch".to_string(),
        // Resolve `${VAR}` / `$VAR` against the build-global ARG scope:
        // `FROM myapp:${VERSION}`. `pull_into_layout` validates the
        // resolved reference, so a placeholder that didn't resolve to a valid
        // ref surfaces as a clear pull error.
        FromSource::Reference(spanned) => {
            umf_core::subst::substitute(spanned.value.as_str(), |n| {
                ctx.globals.get(n).map(String::as_str)
            })
        }
    };

    info!(from = %from_ref_str, stage_ref = %stage_ref, "engine build: stage starting");
    // `scratch` is a keyword, not a reference — there is no image to pull
    // or to resolve from the layout.
    let canonical_from = if scratch {
        from_ref_str.clone()
    } else {
        pull_into_layout(layout, &from_ref_str).await?
    };

    // Pre-pull every external `ADD <oci-ref>` source while we're in async
    // context — the directive walk below is synchronous and reads them from the
    // layout. The reference is `${VAR}`-substituted against the stage's `ARG`
    // scope as it stands at that directive; this walk mirrors the
    // real directive walk — seeded from the build globals, extended positionally
    // by in-stage `ARG` — so `apply_add_oci_image`'s own substitution produces
    // the same string and the lookup keys agree. `apply_add_oci_image`
    // re-canonicalizes the same way (`Reference::whole`); a ref already in the
    // layout short-circuits without touching the network.
    let mut prepull_args = ctx.globals.clone();
    for directive in &stage.directives {
        if let Directive::Arg(arg) = directive {
            crate::arg_subst::apply_arg_to_scope(&mut prepull_args, arg, ctx.build_args);
        } else if let Directive::Add(add) = directive
            // `COPY` (plain_copy) never pulls an OCI image — `apply_add` rejects
            // the remote source — so don't pre-pull it into the layout here.
            && !add.plain_copy
            && let umf_core::ast::AddSource::Oci(spanned) = &add.source
        {
            let resolved = crate::arg_subst::subst_with(&prepull_args, spanned.value.as_str());
            pull_into_layout(layout, &resolved).await?;
        }
    }

    let bundle_opts =
        BundleOptions::for_host("umf-build", LayerStrategy::from_env(), ctx.architecture);
    let step_total = u32::try_from(stage.directives.len()).unwrap_or(u32::MAX);

    // Build the stage with the step cache enabled. If a RUN misses the cache
    // after an earlier step was adopted from it, the overlay lower stack is
    // incomplete (see `BuildState::adopted_from_cache`); we discard the attempt
    // and rebuild with cache *lookups* disabled so every step re-executes
    // against a correct overlay. Stores stay on, so the cache repopulates and
    // the next identical build is an all-hit fast path. At most one rebuild.
    let mut lookup_cache = true;
    let state = loop {
        let (base, mut bundle) = if scratch {
            (
                scratch_base_image(ctx.architecture),
                Bundle::from_scratch(&bundle_opts)?,
            )
        } else {
            (
                resolve_base_image(layout, &canonical_from, ctx.architecture)?,
                Bundle::from_image(layout, &canonical_from, &bundle_opts)?,
            )
        };
        let mut state = BuildState::new(base, ctx.compression, ctx.globals);
        let mut rebuild_without_cache = false;
        let mut step_n: u32 = 0;

        for directive in &stage.directives {
            step_n += 1;
            if let Some(h) = hook {
                let info = umf_engine::StepInfo {
                    stage_index,
                    stage_total,
                    step_index: step_n,
                    step_total,
                    kind: classify_directive(directive),
                    description: describe_directive(directive),
                };
                if matches!(h.before_step(&info), umf_engine::HookAction::Abort) {
                    return Err(EngineBuildError::Engine(EngineError::BuildAborted {
                        stage_index,
                        step_index: step_n,
                    }));
                }
            }
            let flow = apply_directive(
                ctx,
                &mut state,
                &mut bundle,
                directive,
                step_n,
                lookup_cache,
            )?;
            if matches!(flow, StepFlow::RebuildWithoutCache) {
                rebuild_without_cache = true;
                break;
            }
            if let Some(h) = hook {
                let info = umf_engine::StepInfo {
                    stage_index,
                    stage_total,
                    step_index: step_n,
                    step_total,
                    kind: classify_directive(directive),
                    description: describe_directive(directive),
                };
                h.after_step(&info);
            }
        }

        if rebuild_without_cache {
            debug!(
                stage_ref = %stage_ref,
                "engine build: partial cache hit would build on an incomplete overlay; rebuilding stage without cache lookups"
            );
            lookup_cache = false;
            continue;
        }
        break state;
    };

    let final_config = state.finalise_image_config();
    let final_layers = state.assemble_layer_chain()?;

    info!(
        layers = final_layers.len(),
        ref_name = stage_ref,
        "engine build: emitting stage image"
    );
    let entry = emit_image(layout, &final_layers, &final_config, stage_ref)?;
    Ok(entry)
}

#[cfg(test)]
mod tests;
