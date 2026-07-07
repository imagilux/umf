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
    // Container-side mount targets of the secrets, so their placeholder inodes
    // can be scrubbed from the captured upper after the RUN (see below).
    let mut secret_targets: Vec<PathBuf> = Vec::new();
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
                secret_targets.push(container_path.clone());
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

    // Scrub secret bind-mount placeholders from the captured upper before it
    // becomes a layer. The runtime creates a mountpoint inode (an empty file/
    // dir) at each secret target, which persists in the upper once the mount is
    // torn down. The secret *bytes* never touch the upper (they live behind the
    // read-only bind mount), but the empty placeholder — and any `/run/secrets`
    // tree created solely for it — must not ship in the emitted layer.
    scrub_secret_mountpoints(overlay.upper(), &secret_targets);

    let upper = overlay.persist_upper()?;
    let layer = state.push_new_layer(upper, history_summary.clone())?;
    // Store unconditionally (even on a cache-lookups-disabled rebuild) so the
    // cache repopulates and the next identical build is an all-hit fast path.
    store_layer_cache(cache, &key, layer, history_summary)?;
    Ok(StepFlow::Continue)
}

/// Remove secret bind-mount placeholders from a captured upper-dir.
///
/// For each `container_path` target (mapped into `upper`), delete the mountpoint
/// inode the runtime created, then walk up removing now-empty ancestor
/// directories (so a `/run/secrets` created solely for the mount doesn't ship),
/// stopping at the first non-empty directory or the upper root. Best-effort: a
/// failure to remove a placeholder is not fatal (the value never reached the
/// upper), so errors are ignored. A target with a `..` component is skipped
/// rather than followed out of the upper.
fn scrub_secret_mountpoints(upper: &Path, targets: &[PathBuf]) {
    for target in targets {
        let rel = target.strip_prefix("/").unwrap_or(target);
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        let in_upper = upper.join(rel);
        match std::fs::symlink_metadata(&in_upper) {
            Ok(m) if m.is_dir() => {
                let _ = std::fs::remove_dir_all(&in_upper);
            }
            Ok(_) => {
                let _ = std::fs::remove_file(&in_upper);
            }
            Err(_) => continue,
        }
        // Prune now-empty ancestors up to (not including) the upper root.
        let mut cursor = in_upper.parent().map(Path::to_path_buf);
        while let Some(dir) = cursor {
            if dir == upper || !dir.starts_with(upper) {
                break;
            }
            let is_empty = match std::fs::read_dir(&dir) {
                Ok(mut entries) => entries.next().is_none(),
                Err(_) => false,
            };
            if !is_empty || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            cursor = dir.parent().map(Path::to_path_buf);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::scrub_secret_mountpoints;
    use std::path::PathBuf;

    #[test]
    fn scrubs_placeholder_and_prunes_empty_run_secrets() {
        let upper = tempfile::tempdir().unwrap();
        // The empty mountpoint the runtime leaves behind: /run/secrets/token.
        let secrets_dir = upper.path().join("run/secrets");
        std::fs::create_dir_all(&secrets_dir).unwrap();
        std::fs::write(secrets_dir.join("token"), b"").unwrap();

        scrub_secret_mountpoints(upper.path(), &[PathBuf::from("/run/secrets/token")]);

        assert!(!upper.path().join("run/secrets/token").exists());
        assert!(!upper.path().join("run/secrets").exists());
        assert!(!upper.path().join("run").exists(), "empty ancestor pruned");
    }

    #[test]
    fn keeps_ancestor_with_other_content() {
        let upper = tempfile::tempdir().unwrap();
        let secrets_dir = upper.path().join("run/secrets");
        std::fs::create_dir_all(&secrets_dir).unwrap();
        std::fs::write(secrets_dir.join("token"), b"").unwrap();
        // Something the RUN legitimately wrote under /run.
        std::fs::write(upper.path().join("run/other"), b"keep").unwrap();

        scrub_secret_mountpoints(upper.path(), &[PathBuf::from("/run/secrets/token")]);

        assert!(
            !upper.path().join("run/secrets").exists(),
            "empty secrets dir pruned"
        );
        assert!(
            upper.path().join("run/other").exists(),
            "/run kept (has other content)"
        );
    }

    #[test]
    fn ignores_parent_traversal_targets() {
        let upper = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"x").unwrap();
        // A `..`-bearing target must be skipped, never followed out of the upper.
        scrub_secret_mountpoints(upper.path(), &[PathBuf::from("/../../victim")]);
        assert!(victim.exists(), "traversal target must not be touched");
    }
}
