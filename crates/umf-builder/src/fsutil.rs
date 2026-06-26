//! Small filesystem helpers shared across builder targets.

use std::path::Path;

/// Resolve a `WORKDIR` operand against the current working directory, Docker
/// style: an absolute path replaces it; a relative path is appended to it. The
/// default working directory is `/`, so a leading relative `WORKDIR app`
/// resolves to `/app`. Shared by the container builder and the bootable RUN
/// walk so `WORKDIR` means the same on both targets.
pub(crate) fn resolve_workdir(current: &str, requested: &str) -> String {
    if requested.starts_with('/') {
        requested.to_string()
    } else {
        // `trim_end_matches('/')` collapses `current == "/"` to "", yielding
        // `/<requested>`, and avoids a doubled slash for `/a/` + `b`.
        let base = current.trim_end_matches('/');
        format!("{base}/{requested}")
    }
}

/// Recursively copy `src` into `dst`, creating `dst` (and any missing
/// parents) as needed.
///
/// Symlinks are recreated as symlinks — their targets are *not* resolved,
/// so a relative link stays relative and a dangling link stays dangling.
/// Regular files are copied byte-for-byte; directories recurse. Shared by
/// the container build's `ADD` handling and the VM staging copy, which
/// previously each carried an identical copy of this routine.
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(target, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_workdir;

    #[test]
    fn absolute_workdir_replaces_current() {
        assert_eq!(resolve_workdir("/opt/app", "/srv"), "/srv");
        assert_eq!(resolve_workdir("/", "/a/b"), "/a/b");
    }

    #[test]
    fn relative_workdir_joins_onto_current() {
        assert_eq!(resolve_workdir("/opt/app", "sub"), "/opt/app/sub");
        // Default working dir is `/`, so a leading relative WORKDIR is under root.
        assert_eq!(resolve_workdir("/", "app"), "/app");
        // No doubled slash when the current dir has a trailing slash.
        assert_eq!(resolve_workdir("/a/", "b"), "/a/b");
    }
}
