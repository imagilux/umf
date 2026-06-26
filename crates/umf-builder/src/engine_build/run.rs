//! The `RUN` directive handler.
//!
//! Executes a RUN step through the libcontainer runtime against an overlay
//! stacked on the current layer state, captures the resulting upper-dir as a
//! new layer, and folds the step into the content-addressed cache. Split out
//! of [`super::directives`] so that module stays focused on dispatch and the
//! metadata handlers.

use std::path::{Path, PathBuf};

use tracing::info;
use umf_core::ast::{Run, RunCommand};
use umf_engine::bundle::Bundle;
use umf_engine::overlay::Overlay;
use umf_engine::runtime::{ContainerRuntime, RunSpec};

use super::EngineBuildError;
use super::cache::{layer_from_cache, parent_state_hash, run_cache_key};
use super::directives::{BuildCtx, StepFlow, store_layer_cache};
use super::state::BuildState;

pub(super) fn apply_run(
    ctx: &BuildCtx,
    state: &mut BuildState,
    bundle: &mut Bundle,
    run: &Run,
    step_n: u32,
    lookup_cache: bool,
) -> Result<StepFlow, EngineBuildError> {
    let &BuildCtx {
        runtime,
        layout,
        cache,
        secrets,
        architecture,
        ..
    } = ctx;
    // Build argv: exec form → as-is; shell form → current_shell + command.
    // `${VAR}` / `$VAR` are substituted against the ARG scope for the *executed*
    // argv (which also feeds the cache key, so a changed value rebuilds), while
    // the history line keeps the original recipe text — so an ARG value never
    // lands in the image history (the classic build-arg leak).
    let (argv, history_summary) = match &run.command {
        RunCommand::Shell(spanned) => {
            let original = spanned.value.as_str();
            let mut argv = state.current_shell.clone();
            argv.push(state.subst(original));
            (argv, format!("RUN {original}"))
        }
        RunCommand::Exec(parts) => {
            let argv: Vec<String> = parts
                .iter()
                .map(|p| state.subst(p.value.as_str()))
                .collect();
            let summary = format!(
                "RUN {}",
                parts
                    .iter()
                    .map(|p| p.value.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            (argv, summary)
        }
    };

    let env = state.image_config.container.env.clone();
    let cwd = state.image_config.container.working_dir.clone();
    let user = state.image_config.container.user.clone();

    // Build secret bind-mounts requested by this RUN's `--mount=type=secret`
    // options. The cache key folds in the secret IDs (so a recipe edit
    // referencing a different secret busts the cache) but NOT the
    // secret bytes — an unchanged recipe with a rotated secret value
    // still gets a cache hit.
    let mut bind_mounts: Vec<umf_engine::runtime::BindMount> = Vec::new();
    let mut referenced_secret_ids: Vec<String> = Vec::new();
    for mount in &run.mounts {
        match &mount.kind {
            umf_core::ast::RunMountKind::Secret { id, target } => {
                let id_str = id.value.as_str().to_string();
                referenced_secret_ids.push(id_str.clone());
                let host_path = secrets
                    .host_path_for(&id_str)
                    .ok_or_else(|| EngineBuildError::MissingSecret(id_str.clone()))?;
                let container_path = match target {
                    Some(t) => PathBuf::from(t.value.as_str()),
                    None => PathBuf::from(format!("/run/secrets/{id_str}")),
                };
                bind_mounts.push(umf_engine::runtime::BindMount {
                    host_path: host_path.to_path_buf(),
                    container_path,
                    read_only: true,
                });
            }
        }
    }

    // Cache lookup. The key folds in cumulative parent state, argv,
    // env, working dir, user, and the IDs of any referenced secrets.
    let parent = parent_state_hash(state);
    let key = run_cache_key(
        architecture.oci_arch_string(),
        state.compression.media_type(),
        &parent,
        &argv,
        &env,
        cwd.as_deref(),
        user.as_deref(),
        &referenced_secret_ids,
    );
    if lookup_cache
        && let Some(entry) = cache.lookup(&key)
        && let Some(reused) = layer_from_cache(layout, &entry)?
    {
        info!(
            step = step_n,
            "engine build: RUN cache hit (skipping execution)"
        );
        state.adopt_cached_layer(reused, entry.history_line);
        return Ok(StepFlow::Continue);
    }

    // Cache miss (or lookups disabled for this attempt). If an earlier step in
    // this stage was satisfied from cache, its layer is in `new_layers` but not
    // in `upper_guards`, so the overlay below would be missing it — executing
    // here would build against an incomplete filesystem and silently cache the
    // wrong result. Bail; `build_one_stage` rebuilds the stage without cache
    // lookups so every step re-executes against a correct overlay.
    if state.adopted_from_cache {
        return Ok(StepFlow::RebuildWithoutCache);
    }

    // Set up the overlay (stacked lowers = previous uppers + the base
    // image's lower stack) and redirect the bundle's spec at the
    // overlay's merged path. The base lowers are copied to owned PathBufs
    // so the immutable borrow of `bundle` ends before `set_root_path`
    // needs `&mut bundle`. The merge strategy yields one rootfs dir; the
    // erofs strategy yields one mountpoint per base layer.
    let base_lowers: Vec<std::path::PathBuf> = bundle
        .base_lowers()
        .iter()
        .map(|p| p.to_path_buf())
        .collect();
    let base_refs: Vec<&Path> = base_lowers
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect();
    let lowers = state.lower_stack(&base_refs);
    let overlay = Overlay::mount(&lowers)?;
    bundle.set_root_path(overlay.merged())?;

    // Apply image-config-derived runtime context (env, workdir, user)
    // onto the RunSpec so the RUN step sees what the in-progress image
    // declares.
    let spec = RunSpec {
        id: format!("umf-build-step-{step_n}"),
        argv,
        env,
        working_dir: cwd,
        user,
        bind_mounts,
    };

    let outcome = runtime.run(bundle, &spec)?;
    if !outcome.is_success() {
        return Err(EngineBuildError::RunFailed {
            code: outcome.exit_code.unwrap_or(-1),
            summary: history_summary,
        });
    }

    let upper = overlay.persist_upper()?;
    let layer = state.push_new_layer(upper, history_summary.clone())?;
    // Store unconditionally (even on a cache-lookups-disabled rebuild) so the
    // cache repopulates and the next identical build is an all-hit fast path.
    store_layer_cache(cache, &key, layer, history_summary)?;
    Ok(StepFlow::Continue)
}
