//! Default seccomp syscall filter for RUN steps.
//!
//! Every container RUN step the engine launches gets the well-tested
//! containerd/Docker **default** seccomp profile applied to it: a
//! `defaultAction` of `SCMP_ACT_ERRNO` (deny by default) plus a large
//! allowlist of safe syscalls. This blocks the dangerous tail of the
//! syscall surface (`kexec_load`, `bpf`, `mount` outside the few allowed
//! cases, `ptrace` of arbitrary processes, etc.) without breaking ordinary
//! build tooling.
//!
//! The profile JSON in [`DEFAULT_PROFILE_JSON`] is vendored **verbatim**
//! from moby/moby (`profiles/seccomp/default.json`, tag `v27.5.1`), the
//! canonical hand-maintained source for this allowlist that Docker, and via
//! containers/common podman + CRI-O, all ship. We do not hand-write a
//! profile (that is error-prone); we vendor the upstream one and parse it
//! into [`oci_spec::runtime::LinuxSeccomp`].
//!
//! ## Format note
//!
//! moby's JSON carries a few Docker extension fields that are not part of
//! the OCI runtime-spec `LinuxSeccomp` schema: `archMap` (an arch →
//! sub-architecture expansion) and per-syscall `includes`/`excludes`/
//! `comment` gates. `oci_spec` does not set `deny_unknown_fields`, so serde
//! ignores those extension fields cleanly. The practical effect: the parsed
//! profile carries no explicit `architectures` (which per the OCI spec means
//! "applies regardless of architecture"), and a handful of syscall blocks
//! that moby would gate behind an `includes`/`excludes` arch/cap condition
//! apply unconditionally. That only ever *widens* the allowlist by syscalls
//! that are otherwise-safe on the host arch; it never turns an allow into a
//! deny, so the deny-by-default posture is preserved.

use oci_spec::runtime::LinuxSeccomp;

use crate::error::EngineError;

/// The vendored containerd/Docker default seccomp profile, embedded at
/// compile time. Source: moby/moby `profiles/seccomp/default.json` (tag
/// `v27.5.1`). See the module docs for why it is vendored verbatim.
pub const DEFAULT_PROFILE_JSON: &str = include_str!("seccomp/default.json");

/// Parse the vendored default seccomp profile into an OCI
/// [`LinuxSeccomp`] ready to attach to a runtime spec.
///
/// A parse failure here is a build-time bug (the embedded JSON is fixed and
/// verified by a unit test), not a runtime/host condition, so it surfaces as
/// a clear [`EngineError`] rather than being silently dropped.
///
/// # Errors
/// [`EngineError::Runtime`] if the embedded JSON cannot be deserialised into
/// [`LinuxSeccomp`] (only reachable if the vendored resource is corrupted).
pub fn default_profile() -> Result<LinuxSeccomp, EngineError> {
    serde_json::from_str(DEFAULT_PROFILE_JSON).map_err(|e| {
        EngineError::runtime(
            format!(
                "vendored default seccomp profile (crates/umf-engine/src/seccomp/default.json) \
                 failed to parse into oci_spec::runtime::LinuxSeccomp: {e}"
            ),
            Some(Box::new(e)),
        )
    })
}

#[cfg(test)]
mod tests;
