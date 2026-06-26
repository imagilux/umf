//! Small mount/unmount helpers shared by the overlay and erofs mount
//! plumbing.
//!
//! Both [`crate::overlay`] (fuse-overlayfs) and [`crate::erofs`]
//! (`erofsfuse`) tear down their FUSE mounts the same way — try the
//! modern `fusermount3 -u`, fall back to `fusermount -u`. Factoring it
//! here keeps the candidate list and the success check in one place.

use std::path::Path;
use std::process::Command;

/// Unmount a FUSE mount at `mp` via `fusermount3 -u`, falling back to
/// `fusermount -u` for distros that haven't migrated to fuse3 yet.
///
/// Returns `true` if either binary unmounted successfully. A `false`
/// return (binary missing, or both invocations failed) is the caller's
/// cue to fall back to a lazy `umount2(MNT_DETACH)`.
pub(crate) fn fusermount_unmount(mp: &Path) -> bool {
    for bin in ["fusermount3", "fusermount"] {
        if Command::new(bin)
            .arg("-u")
            .arg(mp)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}
