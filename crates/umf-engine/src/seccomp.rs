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
//! ## Format note — the cap/arch gates matter
//!
//! moby's JSON carries Docker extension fields that are not part of the OCI
//! runtime-spec `LinuxSeccomp` schema: `archMap` (an arch → sub-architecture
//! expansion) and per-syscall `includes`/`excludes`/`comment` gates.
//! `oci_spec` does not set `deny_unknown_fields`, so serde parses the profile
//! but **silently drops** those gate fields.
//!
//! Dropping the gates is **not** safe. moby uses `includes.caps` /
//! `excludes.caps` to keep dangerous syscalls *denied* unless the container
//! holds the matching capability: the block
//! `{ names: [unshare, setns, mount, clone, clone3, bpf, open_by_handle_at, …],
//! includes: { caps: [CAP_SYS_ADMIN] } }` is a **conditional** allow — it
//! applies only when the container has `CAP_SYS_ADMIN`. Parsed with the gate
//! dropped it becomes an *unconditional* allow, so a RUN step that holds none
//! of those caps (umf grants the conservative Docker default set — no
//! `CAP_SYS_ADMIN`) can still `unshare(CLONE_NEWUSER)` / `setns` / `mount`,
//! re-opening the nested-userns escape the profile is meant to block.
//!
//! So [`filtered_profile`] evaluates the gates the way moby does — against the
//! container's actual capability set and target architecture — and keeps only
//! the blocks that apply, letting the rest fall through to the deny-by-default
//! action. It also sets the seccomp `architectures` (primary + compat
//! sub-arches) so the filter covers the 32-bit/compat syscall ABI rather than
//! leaving a native-only filter bypassable through the compat entry point.
//!
//! On amd64/arm64 `clone` itself is *only* in the `CAP_SYS_ADMIN`-gated block
//! plus an `excludes: { caps: [CAP_SYS_ADMIN] }` block that carries an argument
//! filter forbidding the `CLONE_NEW*` namespace flags; evaluating the gates
//! keeps that arg-restricted `clone` (so ordinary `fork`/`exec` still works)
//! while dropping the unrestricted one — exactly Docker's no-`CAP_SYS_ADMIN`
//! posture.

use std::collections::HashSet;

use oci_spec::runtime::{Arch, Capability, LinuxSeccomp};
use serde::Deserialize;
use serde_json::Value;
use umf_core::architecture::Architecture;

use crate::error::EngineError;

/// The vendored containerd/Docker default seccomp profile, embedded at
/// compile time. Source: moby/moby `profiles/seccomp/default.json` (tag
/// `v27.5.1`). See the module docs for why it is vendored verbatim.
pub const DEFAULT_PROFILE_JSON: &str = include_str!("seccomp/default.json");

fn parse_error(e: serde_json::Error) -> EngineError {
    EngineError::runtime(
        format!(
            "vendored default seccomp profile (crates/umf-engine/src/seccomp/default.json) \
             failed to parse: {e}"
        ),
        Some(Box::new(e)),
    )
}

/// Parse the vendored default seccomp profile *verbatim*, gates un-evaluated.
///
/// Retained for the resource-integrity test; production RUN steps go through
/// [`filtered_profile`], which evaluates the cap/arch gates. Parsing the raw
/// profile without a capability context would leave the `CAP_SYS_ADMIN`-gated
/// syscalls unconditionally allowed (see the module docs).
///
/// # Errors
/// [`EngineError::Runtime`] if the embedded JSON cannot be deserialised into
/// [`LinuxSeccomp`] (only reachable if the vendored resource is corrupted).
pub fn default_profile() -> Result<LinuxSeccomp, EngineError> {
    serde_json::from_str(DEFAULT_PROFILE_JSON).map_err(parse_error)
}

/// A moby syscall-block gate (`includes` / `excludes`). `minKernel` is
/// intentionally not modelled: every `minKernel` in the vendored profile is an
/// old floor (≤ 4.8) that any umf-supported host (KVM + youki + overlayfs)
/// clears, so an includes-gate on it is always satisfied and no excludes-gate
/// on it exists.
#[derive(Debug, Default, Deserialize)]
struct Gate {
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    arches: Vec<String>,
}

/// The `CAP_*` strings the container actually holds. `oci_spec::Capability`
/// serialises to exactly moby's gate spelling (`CAP_SYS_ADMIN`, …).
fn held_caps(caps: &HashSet<Capability>) -> HashSet<String> {
    caps.iter()
        .filter_map(|c| serde_json::to_value(c).ok())
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// moby GOARCH token used in the profile's `includes`/`excludes` `arches`.
fn moby_arch(arch: Architecture) -> &'static str {
    match arch {
        Architecture::X86_64 => "amd64",
        Architecture::Aarch64 => "arm64",
    }
}

/// seccomp architecture set for the target — primary plus compat sub-arches
/// (matching moby's `archMap`) — so a deny-by-default filter also covers the
/// 32-bit / compat syscall ABI rather than being bypassable through it.
fn seccomp_arches(arch: Architecture) -> Vec<Arch> {
    match arch {
        Architecture::X86_64 => vec![Arch::ScmpArchX86_64, Arch::ScmpArchX86, Arch::ScmpArchX32],
        Architecture::Aarch64 => vec![Arch::ScmpArchAarch64, Arch::ScmpArchArm],
    }
}

fn gate_of(block: &Value, key: &str) -> Gate {
    block
        .get(key)
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Whether a syscall block applies, per moby's `setupSeccomp` gate semantics:
/// an `excludes` match (arch in the list, or any listed cap held) drops the
/// block; an `includes` gate requires the arch to match (when listed) and every
/// listed cap to be held.
fn block_applies(block: &Value, held: &HashSet<String>, arch: &str) -> bool {
    let inc = gate_of(block, "includes");
    let exc = gate_of(block, "excludes");
    if !exc.arches.is_empty() && exc.arches.iter().any(|a| a == arch) {
        return false;
    }
    if exc.caps.iter().any(|c| held.contains(c)) {
        return false;
    }
    if !inc.arches.is_empty() && !inc.arches.iter().any(|a| a == arch) {
        return false;
    }
    if !inc.caps.iter().all(|c| held.contains(c)) {
        return false;
    }
    true
}

/// Build the effective seccomp profile for a RUN step whose process holds
/// `caps` on target `arch`, evaluating moby's cap/arch gates (which `oci_spec`
/// would otherwise drop) and pinning the architecture set. See the module docs.
///
/// # Errors
/// [`EngineError::Runtime`] if the vendored profile can't be parsed.
pub fn filtered_profile(
    caps: &HashSet<Capability>,
    arch: Architecture,
) -> Result<LinuxSeccomp, EngineError> {
    let mut root: Value = serde_json::from_str(DEFAULT_PROFILE_JSON).map_err(parse_error)?;
    let held = held_caps(caps);
    let tok = moby_arch(arch);
    if let Some(Value::Array(syscalls)) = root.get_mut("syscalls") {
        syscalls.retain(|block| block_applies(block, &held, tok));
    }
    // The remaining Docker extension fields (`archMap`, per-syscall
    // `includes`/`excludes`/`comment`) are ignored by `oci_spec` on parse; the
    // `args` filters we must keep (e.g. clone's namespace-flag mask) are.
    let mut profile: LinuxSeccomp = serde_json::from_value(root).map_err(parse_error)?;
    profile.set_architectures(Some(seccomp_arches(arch)));
    Ok(profile)
}

#[cfg(test)]
mod tests;
