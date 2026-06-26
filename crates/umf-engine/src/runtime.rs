//! [`ContainerRuntime`] trait — the seam between bundle prep and the
//! underlying OCI runtime (youki/`libcontainer`, runc shell-out, …).
//!
//! Backends own one job: given a prepared OCI bundle (rootfs +
//! config.json), execute the bundle's `process.args` to completion,
//! return the exit status plus the captured stdout/stderr. **Bundle
//! creation, overlay setup, and upper-dir capture are the caller's
//! responsibility** — bundle prep is in [`crate::bundle`], overlay
//! setup is in [`crate::overlay`]. The runtime backend doesn't know or
//! care whether the bundle's rootfs is a flat directory or a mounted
//! overlay's merged view; from its perspective it's just a path.
//!
//! The caller flow for a single build step:
//!
//! 1. Construct (or re-use) a [`Bundle`] with its `root.path` pointing
//!    at the rootfs the container should see.
//! 2. Call `runtime.run(&mut bundle, &spec)`.
//! 3. If the rootfs was an overlay, persist its upper-dir after the
//!    run completes — that's the captured layer diff.

use std::path::PathBuf;

use crate::bundle::Bundle;
use crate::error::EngineError;

/// What the engine should run inside the prepared bundle.
///
/// The bundle's `config.json` already encodes the rootfs path, the
/// uid/gid mappings, the namespace set, and most of the security profile.
/// `RunSpec` carries the bits that *vary per RUN step* — the command, the
/// env, the working directory, the user override — so the bundle can be
/// reused across RUN steps when caching allows.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// The container ID — used by the runtime to track the running
    /// instance and (for `libcontainer`) name its state directory. Must
    /// be unique within the runtime's `state_root`.
    pub id: String,
    /// argv passed to `execvp` inside the container.
    pub argv: Vec<String>,
    /// Environment variables (`KEY=VALUE` strings, OCI runtime-spec
    /// convention). These are *appended* to whatever the bundle's
    /// config.json already declares; duplicate keys take the
    /// `RunSpec`-supplied value.
    pub env: Vec<String>,
    /// Working directory inside the container. `None` ⇒ inherit from
    /// the bundle's config.json.
    pub working_dir: Option<String>,
    /// User override (`uid` or `uid:gid`). `None` ⇒ inherit.
    pub user: Option<String>,
    /// Bind-mounts to inject into the container's runtime spec before
    /// it starts. Used by the builder to expose
    /// `RUN --mount=type=secret` material at a known path inside the
    /// container without writing it into a layer.
    pub bind_mounts: Vec<BindMount>,
}

impl RunSpec {
    /// Convenience constructor for the common case (id + argv only).
    #[must_use]
    pub fn new(id: impl Into<String>, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            id: id.into(),
            argv: argv.into_iter().map(Into::into).collect(),
            env: Vec::new(),
            working_dir: None,
            user: None,
            bind_mounts: Vec::new(),
        }
    }
}

/// One bind-mount injected into the container at run time.
///
/// Used for `RUN --mount=type=secret` and (in the future) for any other
/// caller-driven bind-mount that needs to appear inside the container
/// without ending up in a layer.
///
/// The host path must exist when [`ContainerRuntime::run`] is called —
/// the caller is responsible for materialising secret content into a
/// file on disk first (e.g. via [`tempfile::NamedTempFile`]) and
/// removing it after the run.
#[derive(Debug, Clone)]
pub struct BindMount {
    /// Absolute host path of the source file / directory.
    pub host_path: PathBuf,
    /// Absolute container path where the source appears.
    pub container_path: PathBuf,
    /// `true` to mount read-only (the default for secrets); `false` for
    /// read-write.
    pub read_only: bool,
}

/// Build an OCI runtime-spec [`Mount`](oci_spec::runtime::Mount) from a
/// [`BindMount`].
///
/// Shared by every backend that injects caller-supplied bind-mounts (the
/// build path's per-RUN secrets and `umf run`'s `--bind`), so the
/// `bind,rprivate` option set and the explicit `ro`/`rw` flag are derived
/// in exactly one place.
///
/// # Errors
/// [`EngineError::Runtime`] if the OCI `MountBuilder` rejects the inputs.
pub(crate) fn bind_mount(bm: &BindMount) -> Result<oci_spec::runtime::Mount, EngineError> {
    let options = vec![
        "bind".to_string(),
        "rprivate".to_string(),
        if bm.read_only { "ro" } else { "rw" }.to_string(),
    ];
    oci_spec::runtime::MountBuilder::default()
        .destination(&bm.container_path)
        .typ("bind".to_string())
        .source(&bm.host_path)
        .options(options)
        .build()
        .map_err(|e| EngineError::runtime(format!("MountBuilder rejected bind mount: {e}"), None))
}

/// What the runtime reports back after the container exits.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Process exit status. `Some(0)` ⇒ clean exit; `Some(n)` ⇒ non-zero
    /// exit; `None` ⇒ killed by signal (the runtime couldn't recover the
    /// numeric code).
    pub exit_code: Option<i32>,
    /// Captured stdout bytes. Empty if the backend didn't capture.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes. Empty if the backend didn't capture.
    pub stderr: Vec<u8>,
}

impl RunOutcome {
    /// Whether the run completed with a zero exit code. `None` (killed
    /// by signal) is *not* considered success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// A container runtime that can execute a prepared bundle.
///
/// Implementors are usually thin wrappers around an existing OCI runtime
/// (libcontainer, runc, crun). The trait is intentionally narrow so any
/// such wrapper fits.
///
/// Backends are responsible for:
///
/// - Pointing the runtime at the bundle (`bundle.path()`).
/// - Mutating the bundle's spec to reflect `RunSpec` (argv, env, working
///   dir, user). The bundle is `&mut` for this reason — backends may
///   rewrite `config.json` in place before calling the runtime.
/// - Starting the container, waiting for it to exit, collecting status
///   and (if possible) stdout/stderr.
/// - Cleaning up any runtime-side state (`Container::delete`, state-dir
///   removal). Bundle disk lifetime is the caller's concern; the bundle's
///   own `TempDir` handles that.
pub trait ContainerRuntime {
    /// Execute `spec` inside `bundle`, return what happened.
    ///
    /// # Errors
    /// Backend-specific. The engine maps backend errors into
    /// [`EngineError::Runtime`] with the original error as `source`.
    fn run(&self, bundle: &mut Bundle, spec: &RunSpec) -> Result<RunOutcome, EngineError>;
}
