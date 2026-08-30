//! OCI whiteout markers — one definition shared by the layer reader
//! ([`crate::materialize`]) and the layer writer ([`crate::image`]).
//!
//! A layer records a deletion as a `.wh.<name>` marker file and a
//! "forget everything the lower layers put here" as a `.wh..wh..opq` marker
//! inside the directory. overlayfs, which is what actually produces the
//! upper-dir we pack, uses a *different* encoding for both — a 0/0 character
//! device for a deletion, an xattr for an opaque directory — so the writer
//! has to translate. Keeping the constants and the translation in one module
//! is what stops the two halves drifting apart.

use std::path::Path;

/// OCI whiteout prefix: `.wh.<name>` deletes `<name>` from the lower layers.
pub(crate) const WH_PREFIX: &str = ".wh.";

/// OCI opaque marker: `.wh..wh..opq` clears the containing directory's
/// lower-layer contents before this layer's entries apply.
pub(crate) const WH_OPAQUE: &str = ".wh..wh..opq";

/// The xattr names an overlay backend may use to mark a directory opaque.
///
/// Which one appears depends on how the overlay was mounted, so all three are
/// checked rather than assuming a backend:
///
/// - `trusted.overlay.opaque` — kernel overlayfs mounted with real privilege.
///   Reading a `trusted.*` xattr needs `CAP_SYS_ADMIN`, which the rootful
///   build path has and the rootless one does not.
/// - `user.overlay.opaque` — kernel overlayfs mounted inside a user namespace
///   (supported since Linux 5.11), which stores its metadata under `user.*`
///   precisely because `trusted.*` is unavailable there.
/// - `user.fuseoverlayfs.opaque` — the `fuse-overlayfs` backend UMF falls back
///   to when the effective uid is not 0.
pub(crate) const OPAQUE_XATTRS: &[&str] = &[
    "trusted.overlay.opaque",
    "user.overlay.opaque",
    "user.fuseoverlayfs.opaque",
];

/// The `.wh.`-prefixed marker name that deletes `name` from the lower layers.
pub(crate) fn whiteout_name(name: &str) -> String {
    format!("{WH_PREFIX}{name}")
}

/// `true` if `path` is an overlayfs deletion marker: a character device whose
/// device number is 0/0.
///
/// This is how both kernel overlayfs and a privileged `fuse-overlayfs` record
/// "this path is deleted relative to the lower layers". It is deliberately
/// narrow — a real character device in an image (`/dev/null` is 1/3) has a
/// non-zero rdev and is left alone.
pub(crate) fn is_overlay_deletion(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// `true` if `dir` carries an overlay opaque-directory marker.
///
/// A failure to read the xattr (unsupported filesystem, missing capability)
/// is treated as "not opaque": the alternative is failing a build over a
/// marker we merely cannot see, and the cost of a false negative is a
/// directory that merges with its lower instead of replacing it.
pub(crate) fn is_opaque_dir(dir: &Path) -> bool {
    OPAQUE_XATTRS
        .iter()
        .any(|name| matches!(xattr::get(dir, name), Ok(Some(value)) if value == b"y"))
}

#[cfg(test)]
mod tests;
