//! User-facing "run a pre-built image" entrypoint.
//!
//! Whereas [`crate::backends::libcontainer`] is the per-RUN-step executor
//! used by build pipelines, [`run_image`] is the path that powers
//! `umf run <ref>` from the CLI: it materialises a finished image into
//! an OCI bundle, applies the image-config's ENTRYPOINT + CMD + ENV +
//! WORKDIR (with optional caller overrides on top), drives libcontainer
//! through create → start → wait, and returns the exit code.
//!
//! ## Why a separate entrypoint
//!
//! [`LibcontainerRuntime`](crate::backends::LibcontainerRuntime)'s `run` is
//! designed for the build path: it takes a [`RunSpec`](crate::RunSpec)
//! supplied by the caller (`umf-builder`) for one specific RUN step. The
//! run-path doesn't have a recipe-supplied RunSpec — it has an image
//! whose ENTRYPOINT/CMD are the program to execute. Rather than
//! reverse-engineer a synthetic `RunSpec` from the image config at the
//! call site, [`run_image`] handles the image → spec translation
//! internally and exposes the small surface a CLI actually needs
//! ([`RunOptions`]).
//!
//! ## Override semantics
//!
//! Mirrors the convention every container CLI follows:
//!
//! | Caller passes                                  | argv used                              |
//! |------------------------------------------------|----------------------------------------|
//! | Nothing                                        | image `Entrypoint` + image `Cmd`       |
//! | Only [`RunOptions::cmd_override`]              | image `Entrypoint` + override          |
//! | Only [`RunOptions::entrypoint_override`]       | override (image `Cmd` is dropped)      |
//! | Both                                           | override + override                    |
//!
//! Env merges: image env first, caller overrides applied on top (later
//! wins on duplicate `KEY=`).

use std::path::{Path, PathBuf};

use tracing::{debug, warn};
use umf_oci::registry::ImageLayout;

use crate::backends::libcontainer::drive_container;
use crate::bundle::{Bundle, BundleOptions, LayerStrategy};
use crate::env::merge_env;
use crate::error::EngineError;
use crate::overlay::Overlay;
use crate::runtime::{BindMount, bind_mount};

/// Caller overrides for [`run_image`].
///
/// Every field is optional or zero-valued; `RunOptions::default()` runs
/// the image with its baked-in defaults (ENTRYPOINT, CMD, ENV, WORKDIR
/// from the image config) and lets libcontainer pick a sensible
/// state-root.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Replace the image's `Entrypoint`. When `Some`, the value is used
    /// verbatim; the image's `Cmd` is dropped (mirrors the standard
    /// `--entrypoint` convention).
    pub entrypoint_override: Option<Vec<String>>,
    /// Replace the image's `Cmd`. When `Some` *and*
    /// [`Self::entrypoint_override`] is `None`, the result is image
    /// `Entrypoint` + this value. When both are `Some`, they are
    /// concatenated in order.
    pub cmd_override: Option<Vec<String>>,
    /// Extra environment in `KEY=VAL` form. Merged into the image's env
    /// — duplicates take the caller-supplied value.
    pub env_overrides: Vec<String>,
    /// Allocate a pseudo-TTY for the container. Sets
    /// `process.terminal = true` in the runtime spec so libcontainer
    /// wires up pty + stdio passthrough.
    pub interactive: bool,
    /// Where libcontainer stores per-container state files. `None`
    /// falls back to `$XDG_RUNTIME_DIR/umf-engine/run` (or
    /// `/run/umf-engine/run`).
    pub state_root: Option<PathBuf>,
    /// Explicit container id. `None` ⇒ generated as
    /// `umf-run-<pid>-<nanos>`.
    pub container_id: Option<String>,
    /// Bind-mounts injected into the runtime spec before start. Same
    /// shape as the build path's [`BindMount`].
    pub bind_mounts: Vec<BindMount>,
    /// Preserve the on-disk bundle after the run completes. When
    /// `false`, the bundle's tempdir is cleaned up on drop. When
    /// `true`, the bundle is leaked and its path is returned in
    /// [`RunResult::bundle_path`] for the caller to display.
    pub keep_bundle: bool,
}

/// What [`run_image`] reports back.
#[derive(Debug)]
pub struct RunResult {
    /// Process exit code. `Some(n)` for a clean exit; `Some(128 + sig)`
    /// when the container was killed by a signal. `None` only if
    /// `waitpid` returned an unrecognised status.
    pub exit_code: Option<i32>,
    /// Absolute path to the preserved bundle directory. `Some` only
    /// when [`RunOptions::keep_bundle`] was set.
    pub bundle_path: Option<PathBuf>,
}

/// Execute a pre-built image from `layout`.
///
/// Pipeline:
///
/// 1. Resolve `ref_name` against the layout and prep an OCI bundle.
/// 2. Apply `options` on top of the bundle's seeded spec (argv from
///    image + overrides, env merge, terminal, bind-mounts).
/// 3. Flush the spec to `config.json`.
/// 4. Drive libcontainer through `as_init → build → start → waitpid →
///    delete`.
/// 5. Return the exit code (and the preserved bundle path when
///    `keep_bundle` was set).
///
/// # Errors
/// Any [`EngineError`] from bundle prep, spec construction, or the
/// libcontainer driver.
#[tracing::instrument(
        level = "info",
        name = "umf.engine.run_image",
    skip(layout, options),
    fields(
        ref_name = %ref_name,
        interactive = options.interactive,
        keep_bundle = options.keep_bundle,
    )
)]
pub fn run_image(
    layout: &ImageLayout,
    ref_name: &str,
    options: &RunOptions,
) -> Result<RunResult, EngineError> {
    // `--keep-bundle` wants an inspectable on-disk rootfs, so force the
    // merge-unpack strategy there; otherwise honour UMF_LAYER_CACHE (erofs
    // by default when the host supports it).
    let strategy = if options.keep_bundle {
        LayerStrategy::Merge
    } else {
        LayerStrategy::from_env()
    };
    let mut bundle = Bundle::from_image(
        layout,
        ref_name,
        &BundleOptions {
            layer_strategy: strategy,
            ..BundleOptions::default()
        },
    )?;

    // erofs base lowers are read-only — present a writable overlay as the
    // container root (run writes are ephemeral). On any failure (e.g. no
    // overlay backend), fall back to a merge-unpacked bundle so `umf run`
    // never regresses. The overlay guard is held for the whole run.
    let _overlay = if bundle.uses_erofs_lowers() {
        match mount_run_overlay(&mut bundle) {
            Ok(ov) => Some(ov),
            Err(e) => {
                warn!(error = %e, "erofs run overlay unavailable; falling back to unpack");
                bundle = Bundle::from_image(layout, ref_name, &BundleOptions::default())?;
                None
            }
        }
    } else {
        None
    };

    apply_options_to_bundle(&mut bundle, options)?;

    let container_id = options
        .container_id
        .clone()
        .unwrap_or_else(generate_container_id);

    // Rootless: give `umf run` the same per-container systemd cgroup scope as
    // build RUN steps, so it works inside our user namespace without the fs
    // cgroup manager writing the root-owned `subtree_control`. `None` (fs
    // manager, unchanged) for a host-privileged run. See
    // `rootless::cgroup_scope_path`.
    if let Some(scope) = crate::rootless::cgroup_scope_path(&container_id)
        && let Some(linux) = bundle.spec_mut().linux_mut().as_mut()
    {
        linux.set_cgroups_path(Some(scope));
    }

    // Rootless: hand the container a netns we own and configured (loopback up,
    // plus the selected egress backend) to JOIN via the spec path, since we
    // can't reach into youki's own netns rootless. Held until
    // `run_image` returns, past the container's run; dropped then.
    let _rootless_net = if crate::rootless::context().host_privileged {
        None
    } else {
        crate::backends::libcontainer::setup_rootless_net(&mut bundle)?
    };

    bundle.write_spec()?;

    let state_root = options
        .state_root
        .clone()
        .unwrap_or_else(default_state_root);
    std::fs::create_dir_all(&state_root)?;

    let bundle_path = bundle.path().to_path_buf();
    debug!(
        ref_name,
        container_id = %container_id,
        bundle = %bundle_path.display(),
        "umf run: starting container"
    );

    let exit_code = drive_container(&container_id, &bundle_path, &state_root)?;

    let bundle_path = if options.keep_bundle {
        Some(bundle.into_persistent())
    } else {
        None
    };

    Ok(RunResult {
        exit_code: Some(exit_code),
        bundle_path,
    })
}

/// Mount a writable overlay over the bundle's (read-only erofs) base
/// lowers and repoint the spec's `root.path` at the merged view. Returns
/// the [`Overlay`] guard — keep it alive for the duration of the run; its
/// upper-dir holds the container's ephemeral writes and is discarded on
/// drop.
fn mount_run_overlay(bundle: &mut Bundle) -> Result<Overlay, EngineError> {
    let base: Vec<PathBuf> = bundle
        .base_lowers()
        .iter()
        .map(|p| p.to_path_buf())
        .collect();
    let refs: Vec<&Path> = base.iter().map(PathBuf::as_path).collect();
    let overlay = Overlay::mount(&refs)?;
    bundle.set_root_path(overlay.merged())?;
    Ok(overlay)
}

/// Apply `options` to the bundle's in-memory spec.
///
/// Reads the image's seeded `Entrypoint` / `Cmd` from the bundle, merges
/// the caller's overrides on top, rewrites `process.args` / `process.env`
/// / `process.terminal`, and injects any extra bind-mounts.
fn apply_options_to_bundle(bundle: &mut Bundle, options: &RunOptions) -> Result<(), EngineError> {
    let image_entrypoint = bundle.image_entrypoint().to_vec();
    let image_cmd = bundle.image_cmd().to_vec();
    let argv = compose_argv(
        &image_entrypoint,
        &image_cmd,
        options.entrypoint_override.as_deref(),
        options.cmd_override.as_deref(),
    );
    if argv.is_empty() {
        return Err(EngineError::runtime(
            "image has no ENTRYPOINT or CMD, and no override was supplied — \
             nothing to execute. Pass `--entrypoint <cmd>` or positional args after `--`.",
            None,
        ));
    }

    let spec = bundle.spec_mut();
    let mut process = spec.process().clone().ok_or_else(|| {
        EngineError::runtime(
            "bundle spec has no process section (bundle prep is broken)",
            None,
        )
    })?;

    let merged_env = merge_env(
        process.env().clone().unwrap_or_default(),
        options.env_overrides.iter().cloned(),
    );

    process.set_args(Some(argv));
    process.set_env(Some(merged_env));
    process.set_terminal(Some(options.interactive));

    spec.set_process(Some(process));

    if !options.bind_mounts.is_empty() {
        let mut existing_mounts = spec.mounts().clone().unwrap_or_default();
        for bm in &options.bind_mounts {
            existing_mounts.push(bind_mount(bm)?);
        }
        spec.set_mounts(Some(existing_mounts));
    }

    // The linux namespace set (full PID/NET/IPC/UTS/MNT, plus USER when
    // rootless) is seeded by `bundle::build_runtime_spec`, which always
    // populates `spec.linux()`. We deliberately don't re-derive a weaker
    // set here — overriding it would only ever downgrade the spec.

    Ok(())
}

/// Compose the final argv from image defaults and caller overrides per
/// the table in the module docs.
fn compose_argv(
    image_entrypoint: &[String],
    image_cmd: &[String],
    entrypoint_override: Option<&[String]>,
    cmd_override: Option<&[String]>,
) -> Vec<String> {
    match (entrypoint_override, cmd_override) {
        (Some(ep), Some(cmd)) => {
            let mut out = ep.to_vec();
            out.extend(cmd.iter().cloned());
            out
        }
        (Some(ep), None) => ep.to_vec(),
        (None, Some(cmd)) => {
            let mut out = image_entrypoint.to_vec();
            out.extend(cmd.iter().cloned());
            out
        }
        (None, None) => {
            let mut out = image_entrypoint.to_vec();
            out.extend(image_cmd.iter().cloned());
            out
        }
    }
}

fn default_state_root() -> PathBuf {
    default_state_root_for(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Pure helper for [`default_state_root`] — extracted so tests can exercise
/// the path-construction logic without mutating process-global env vars
/// (the workspace bans `unsafe_code`, and `std::env::set_var` is now
/// `unsafe` on stable).
fn default_state_root_for(xdg_runtime_dir: Option<std::ffi::OsString>) -> PathBuf {
    xdg_runtime_dir
        .map_or_else(|| PathBuf::from("/run"), PathBuf::from)
        .join("umf-engine")
        .join("run")
}

/// Generate a process-unique container id. Format: `umf-run-<pid>-<nanos>`.
/// Nanos protect against repeated invocations within the same process
/// (rare but cheap).
fn generate_container_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("umf-run-{pid}-{nanos}")
}

#[cfg(test)]
mod tests;
