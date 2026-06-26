//! No-op runtime backend.
//!
//! Validates that the bundle's spec accepts the [`crate::runtime::RunSpec`]
//! mutations and writes the updated `config.json`, but does not actually
//! exec anything. Intended for:
//!
//! - Unit testing trait wiring without depending on a real container runtime.
//! - Sanity-checking bundle preparation in environments where namespaces
//!   are unavailable (the libcontainer backend lands separately).
//!
//! Returns `RunOutcome { exit_code: Some(0), stdout: [], stderr: [] }`
//! after mutating the spec; backends doing real execution overwrite this
//! entire pattern with libcontainer/runc calls.

use oci_spec::runtime::{ProcessBuilder, UserBuilder};

use crate::bundle::Bundle;
use crate::env::merge_env;
use crate::error::EngineError;
use crate::runtime::{ContainerRuntime, RunOutcome, RunSpec, bind_mount};

/// A no-op [`ContainerRuntime`] — see module docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopRuntime;

impl ContainerRuntime for NoopRuntime {
    fn run(&self, bundle: &mut Bundle, spec: &RunSpec) -> Result<RunOutcome, EngineError> {
        apply_run_spec_to_bundle(bundle, spec)?;
        bundle.write_spec()?;
        Ok(RunOutcome {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

/// Mutate the bundle's runtime spec to reflect `run_spec`.
///
/// Shared by all backends — they install argv / env / cwd / user the same
/// way; only the actual execution differs. Pulled out here so each backend
/// doesn't reinvent the spec-mutation dance.
pub(crate) fn apply_run_spec_to_bundle(
    bundle: &mut Bundle,
    run_spec: &RunSpec,
) -> Result<(), EngineError> {
    let spec = bundle.spec_mut();
    let process = match spec.process().clone() {
        Some(p) => p,
        None => ProcessBuilder::default().build().map_err(|e| {
            EngineError::runtime(format!("default ProcessBuilder rejected: {e}"), None)
        })?,
    };
    let mut process_builder = ProcessBuilder::default()
        .terminal(process.terminal().unwrap_or(false))
        .args(run_spec.argv.clone());

    let env_combined = merge_env(
        process.env().clone().unwrap_or_default(),
        run_spec.env.iter().cloned(),
    );
    process_builder = process_builder.env(env_combined);

    if let Some(cwd) = run_spec.working_dir.as_ref() {
        process_builder = process_builder.cwd(cwd.clone());
    } else {
        process_builder = process_builder.cwd(process.cwd().to_string_lossy().into_owned());
    }

    if let Some(user_str) = run_spec.user.as_ref() {
        let user = parse_user_spec(user_str)?;
        process_builder = process_builder.user(user);
    } else if let Some(existing_user) = process.user().clone().into() {
        process_builder = process_builder.user(existing_user);
    }

    if let Some(caps) = process.capabilities() {
        process_builder = process_builder.capabilities(caps.clone());
    }
    if let Some(nn) = process.no_new_privileges() {
        process_builder = process_builder.no_new_privileges(nn);
    }

    let new_process = process_builder
        .build()
        .map_err(|e| EngineError::runtime(format!("ProcessBuilder rejected RunSpec: {e}"), None))?;
    spec.set_process(Some(new_process));

    // Append caller-provided bind mounts (e.g. `RUN --mount=type=secret`).
    // The mounts already in the spec (proc, sys, dev, …) stay; we add to
    // them rather than replace, since the secret bind doesn't conflict
    // with the standard set.
    if !run_spec.bind_mounts.is_empty() {
        let mut mounts: Vec<oci_spec::runtime::Mount> = spec.mounts().clone().unwrap_or_default();
        for bm in &run_spec.bind_mounts {
            mounts.push(bind_mount(bm)?);
        }
        spec.set_mounts(Some(mounts));
    }

    // Rootless: name this RUN step's systemd cgroup scope from its (unique) id,
    // so youki's systemd manager creates `umf-<id>.scope` grouped under one
    // `umf.slice`. `None` for a host-privileged build (the fs cgroup manager is
    // left untouched). Per step by design — see `rootless::cgroup_scope_path`.
    if let Some(scope) = crate::rootless::cgroup_scope_path(&run_spec.id)
        && let Some(linux) = spec.linux_mut().as_mut()
    {
        linux.set_cgroups_path(Some(scope));
    }

    Ok(())
}

/// Parse `"uid"` or `"uid:gid"` into an OCI runtime-spec [`User`].
fn parse_user_spec(s: &str) -> Result<oci_spec::runtime::User, EngineError> {
    let (uid_str, gid_str) = s.split_once(':').map_or((s, "0"), |(u, g)| (u, g));
    let uid: u32 = uid_str
        .parse()
        .map_err(|e| EngineError::runtime(format!("invalid uid `{uid_str}`: {e}"), None))?;
    let gid: u32 = gid_str
        .parse()
        .map_err(|e| EngineError::runtime(format!("invalid gid `{gid_str}`: {e}"), None))?;
    UserBuilder::default()
        .uid(uid)
        .gid(gid)
        .build()
        .map_err(|e| EngineError::runtime(format!("UserBuilder rejected: {e}"), None))
}

#[cfg(test)]
mod tests;
