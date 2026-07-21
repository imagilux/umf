//! Subordinate uid/gid delegation for rootless builds.
//!
//! A rootless container build maps container ids onto the caller's **delegated**
//! subordinate ranges (`/etc/subuid` + `/etc/subgid`), applied by the setuid
//! `newuidmap`/`newgidmap` helpers. That is the only way an unprivileged process
//! can map a *range* of ids into its user namespace: the kernel refuses a
//! multi-id map written directly (an unprivileged process may only write a
//! single identity line, which additionally forces `setgroups=deny`). Mapping
//! the full range lets a base image's non-root files, `apt`/`dnf` (which
//! `setgroups()` to a sandbox user), and `RUN --user <nonzero>` all resolve to
//! real ids instead of `nobody`.
//!
//! This module owns parsing the delegation files and computing the map, shared
//! by [`crate::rootless::enter`] (which applies it to umf's own namespace) and
//! [`crate::bundle`] (the youki-driven library-consumer path). It performs no
//! namespace syscalls.

use crate::error::EngineError;

/// A delegated sub-id range parsed from `/etc/subuid` or `/etc/subgid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubIdRange {
    /// First host id in the delegated range.
    pub start: u32,
    /// Number of ids in the range.
    pub count: u32,
}

/// One id-map entry `(container_id, host_id, size)` as `newuidmap`/`newgidmap`
/// take it.
pub type MapTriple = (u32, u32, u32);

/// Resolve the caller's subordinate uid **and** gid ranges, requiring the
/// `newuidmap`/`newgidmap` helpers to be present.
///
/// This is the hard requirement for a rootless container build: there is no
/// single-id fallback, because a single-id map produces subtly-wrong
/// `nobody`-owned output and breaks `setgroups`. A missing helper or delegation
/// is a clear, actionable error rather than a silent degrade.
///
/// # Errors
/// [`EngineError::Runtime`] naming both remedies when either helper is absent
/// from `PATH` or the user has no `/etc/subuid`/`/etc/subgid` entry.
pub fn resolve_ranges(
    host_uid: u32,
    host_gid: u32,
) -> Result<(SubIdRange, SubIdRange), EngineError> {
    if !helpers_present() {
        return Err(missing_delegation_error());
    }
    let username = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(host_uid))
        .ok()
        .flatten()
        .map(|u| u.name);

    let sub_uid = read_subid_range("/etc/subuid", username.as_deref(), host_uid);
    let sub_gid = read_subid_range("/etc/subgid", username.as_deref(), host_gid);

    match (sub_uid, sub_gid) {
        (Some(u), Some(g)) => Ok((u, g)),
        _ => Err(missing_delegation_error()),
    }
}

/// The multi-id map triples `(container_id, host_id, size)` for uid and gid:
/// container `0` maps to the invoking user (so "root-owned" build output lands
/// as the caller on the host), and container `1..1+count` maps onto the
/// delegated range. Pure and unit-testable; the argv fed to the helpers is
/// derived from this by [`helper_args`].
#[must_use]
pub fn mapping_triples(
    euid: u32,
    egid: u32,
    sub_uid: &SubIdRange,
    sub_gid: &SubIdRange,
) -> (Vec<MapTriple>, Vec<MapTriple>) {
    (
        vec![(0, euid, 1), (1, sub_uid.start, sub_uid.count)],
        vec![(0, egid, 1), (1, sub_gid.start, sub_gid.count)],
    )
}

/// Flatten map triples into the `newuidmap`/`newgidmap` argv:
/// `<pid> <cid> <hid> <size> [<cid> <hid> <size> ...]`.
#[must_use]
pub fn helper_args(pid: i32, triples: &[MapTriple]) -> Vec<String> {
    let mut args = Vec::with_capacity(1 + triples.len() * 3);
    args.push(pid.to_string());
    for (container, host, size) in triples {
        args.push(container.to_string());
        args.push(host.to_string());
        args.push(size.to_string());
    }
    args
}

/// The actionable hard-requirement error naming both remedies.
pub fn missing_delegation_error() -> EngineError {
    EngineError::runtime(
        "rootless build requires subordinate uid/gid delegation. Install the `uidmap` \
         package (which provides `newuidmap`/`newgidmap`) and grant your user a range in \
         /etc/subuid and /etc/subgid, e.g. `sudo usermod --add-subuids 100000-165535 \
         --add-subgids 100000-165535 \"$(id -un)\"`. Then re-run, or build as root."
            .to_string(),
        None,
    )
}

/// Read and parse the caller's delegated range from a sub-id file.
fn read_subid_range(path: &str, username: Option<&str>, id: u32) -> Option<SubIdRange> {
    parse_subid_range(&std::fs::read_to_string(path).ok()?, username, id)
}

/// Parse a `/etc/subuid` / `/etc/subgid` body, returning the first range whose
/// owner column matches the resolved `username` or the numeric `id`. Lines are
/// `owner:start:count`; blanks and `#` comments are skipped, zero-count ranges
/// ignored. First match wins (a user with multiple lines gets their first
/// allocation, which is what `newuidmap` validates a single request against).
fn parse_subid_range(contents: &str, username: Option<&str>, id: u32) -> Option<SubIdRange> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split(':');
        let who = parts.next()?;
        let start = parts.next()?.trim().parse::<u32>().ok()?;
        let count = parts.next()?.trim().parse::<u32>().ok()?;
        let matches = username == Some(who) || who.parse::<u32>().ok() == Some(id);
        if matches && count > 0 {
            return Some(SubIdRange { start, count });
        }
    }
    None
}

/// Whether both `newuidmap` and `newgidmap` are present on `PATH`.
#[must_use]
pub fn helpers_present() -> bool {
    binary_on_path("newuidmap") && binary_on_path("newgidmap")
}

/// Whether an executable named `name` exists on `PATH`.
fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

#[cfg(test)]
mod tests;
