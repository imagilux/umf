//! Filesystem writer for the init-system service-enable symlink.
//!
//! Shared between the container target (writes into a synthesised upper-dir
//! that gets packaged as a layer) and the bootable pipeline (writes into the
//! build staging directory); both produce the same on-disk symlink shape, only
//! the surrounding orchestration differs.
//!
//! The sole caller is the `EXPOSE` handler in [`crate::runtime_config`], which
//! enables `nftables.service` so the generated default-deny ruleset actually
//! loads at boot.

use std::fs;
use std::path::{Path, PathBuf};

/// Init system selected by `ENTRYPOINT systemd` / `openrc` — determines the
/// service-enable symlink convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystem {
    /// systemd — units linked under
    /// `/etc/systemd/system/multi-user.target.wants/`.
    Systemd,
    /// OpenRC — units linked under `/etc/runlevels/default/`.
    OpenRc,
}

/// Enable a service unit by dropping the standard "wants" symlink under `root`
/// for the given init system.
///
/// - **systemd**: `etc/systemd/system/multi-user.target.wants/<unit>` →
///   `/usr/lib/systemd/system/<unit>` (absolute — what `systemctl enable` does).
/// - **OpenRC**: `etc/runlevels/default/<unit>` → `/etc/init.d/<unit>`
///   (absolute — what `rc-update add <unit> default` does).
///
/// Replaces any existing symlink at the destination so reapplying is idempotent.
///
/// # Errors
/// Filesystem failure.
pub fn write_enable_link(root: &Path, init: InitSystem, unit: &str) -> std::io::Result<()> {
    let (dir_rel, target) = enable_paths(init, unit);
    let dir = root.join(&dir_rel);
    fs::create_dir_all(&dir)?;
    let link = dir.join(unit);
    if link.exists() || link.is_symlink() {
        fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(&target, &link)
}

/// Where the enable symlink lives + what it points at, per init system.
/// Pure function — useful for tests that want to assert the convention
/// without writing to disk.
#[must_use]
pub fn enable_paths(init: InitSystem, unit: &str) -> (PathBuf, PathBuf) {
    match init {
        InitSystem::Systemd => (
            PathBuf::from("etc/systemd/system/multi-user.target.wants"),
            PathBuf::from(format!("/usr/lib/systemd/system/{unit}")),
        ),
        InitSystem::OpenRc => (
            PathBuf::from("etc/runlevels/default"),
            PathBuf::from(format!("/etc/init.d/{unit}")),
        ),
    }
}

#[cfg(test)]
mod tests;
