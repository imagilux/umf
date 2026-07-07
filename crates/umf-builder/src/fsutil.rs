//! Small filesystem helpers shared across builder targets.

use std::io;
use std::path::{Component, Path, PathBuf};

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
///
/// **Destination containment.** When `dst` (or a subdirectory it merges
/// into) already exists — as it does for the bootable target, whose ADD
/// writes land in one shared staging tree an untrusted userland layer has
/// already populated — a planted symlink at a destination component would be
/// followed by `create_dir_all` / `fs::copy` straight out of the tree. So at
/// every level this refuses to descend into or write through an **existing
/// destination symlink**: a directory `dst` that is a symlink is an error, and
/// a leaf whose path is an existing symlink is unlinked (not followed) before
/// the write. A symlink recreated from `src` is fine — it is never traversed,
/// only re-materialized.
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    // `dst` must be a real directory. If it exists as a symlink, following it
    // would let an earlier untrusted layer redirect the write out of the tree.
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(escape(dst, "destination directory"));
        }
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("copy destination {dst:?} exists and is not a directory"),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir(dst)?,
        Err(e) => return Err(e),
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if kind.is_dir() {
            // The recursive call re-validates `dst_path` against the symlink
            // check above, so a merge into a pre-existing symlinked subdir is
            // rejected there.
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if kind.is_symlink() {
            remove_existing_symlink(&dst_path)?;
            let target = std::fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(target, &dst_path)?;
        } else {
            remove_existing_symlink(&dst_path)?;
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// If `path` currently exists as a symlink, unlink it so a subsequent write
/// creates a fresh entry in place rather than following the link's target out
/// of the containment root. A regular file / directory is left alone (an
/// ordinary overwrite / merge).
fn remove_existing_symlink(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(path),
        _ => Ok(()),
    }
}

fn escape(path: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{what} {path:?} is a symlink; refusing to follow it out of the containment root"),
    )
}

/// Resolve `rel` for **writing** under `root`, creating any missing parent
/// directories, and refusing to traverse an existing symlink at any component.
/// Returns the absolute path to write.
///
/// The bootable target applies every `ADD` into one shared staging tree that an
/// untrusted userland layer (`ADD <oci-ref> /`) has already populated — and a
/// tar layer legitimately ships symlinks. A plain `Path::join` +
/// `create_dir_all` + `fs::copy` would follow a planted `pwn -> /` component
/// out of the staging root and write onto the host filesystem. This descends
/// component-by-component with `symlink_metadata`, creating directories as it
/// goes and erroring on any intermediate symlink; a leaf that already exists as
/// a symlink is unlinked so the caller's write cannot follow it. `..` /
/// absolute / prefix components are rejected outright.
pub(crate) fn contained_write_path(root: &Path, rel: &str) -> io::Result<PathBuf> {
    let mut normals = Vec::new();
    for comp in Path::new(rel.trim_start_matches('/')).components() {
        match comp {
            Component::Normal(c) => normals.push(c),
            Component::CurDir => {}
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("unsafe path component {other:?} in ADD destination {rel:?}"),
                ));
            }
        }
    }
    let mut cur = root.to_path_buf();
    let last = normals.len().saturating_sub(1);
    for (i, c) in normals.iter().enumerate() {
        cur.push(c);
        if i == last {
            // Leaf: don't create it (the caller writes/copies here), but if it
            // already exists as a symlink, unlink it so the write can't follow.
            remove_existing_symlink(&cur)?;
            break;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(escape(&cur, "ADD destination component"));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("ADD destination component {cur:?} exists and is not a directory"),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&cur)?,
            Err(e) => return Err(e),
        }
    }
    Ok(cur)
}

/// Verify a resolved `ADD` **source** path stays inside its root, refusing a
/// symlink that escapes it.
///
/// `resolved` is `root.join(<recipe src>)` (after `..`/absolute rejection) and
/// has already been confirmed to exist. `canonicalize` resolves every symlink
/// component and requires the target to exist, so a producer stage / OCI image
/// that planted `evil -> /` and is read via `ADD --from=evil /evil/etc/shadow`
/// canonicalizes to `/etc/shadow`, which does not start with `root` and is
/// rejected — preventing a host-file read into the emitted layer. An *internal*
/// symlink whose target stays under `root` is allowed (it does not escape).
pub(crate) fn ensure_source_contained(root: &Path, resolved: &Path) -> io::Result<()> {
    let canonical = resolved.canonicalize()?;
    let root_canonical = root.canonicalize()?;
    if !canonical.starts_with(&root_canonical) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "ADD source {resolved:?} resolves to {canonical:?}, outside the source root {root_canonical:?}"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::{
        contained_write_path, copy_dir_recursive, ensure_source_contained, resolve_workdir,
    };
    use std::io::ErrorKind;
    use std::os::unix::fs::symlink;

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

    #[test]
    fn contained_write_creates_parents_and_returns_leaf() {
        let root = tempfile::tempdir().unwrap();
        let p = contained_write_path(root.path(), "/a/b/c.txt").unwrap();
        assert_eq!(p, root.path().join("a/b/c.txt"));
        assert!(root.path().join("a/b").is_dir());
    }

    #[test]
    fn contained_write_rejects_intermediate_symlink_escape() {
        // A prior (untrusted) layer planted `pwn -> <outside>` at the root.
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("pwn")).unwrap();

        let err = contained_write_path(root.path(), "/pwn/etc/backdoor").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        // The escape target was never created.
        assert!(!outside.path().join("etc").exists());
    }

    #[test]
    fn contained_write_unlinks_leaf_symlink_instead_of_following() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"original").unwrap();
        symlink(&victim, root.path().join("leaf")).unwrap();

        let p = contained_write_path(root.path(), "leaf").unwrap();
        std::fs::write(&p, b"new").unwrap();

        // The write landed inside the root, not through the link onto the victim.
        assert_eq!(std::fs::read(root.path().join("leaf")).unwrap(), b"new");
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
    }

    #[test]
    fn contained_write_rejects_parent_traversal() {
        let root = tempfile::tempdir().unwrap();
        let err = contained_write_path(root.path(), "../escape").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn copy_dir_refuses_merge_into_destination_symlink() {
        // dst tree already has `sub -> <outside>` (planted by an earlier layer);
        // copying a src that also has `sub/` must not write through the link.
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/file"), b"payload").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dst.path().join("sub")).unwrap();

        let err = copy_dir_recursive(src.path(), dst.path()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(!outside.path().join("file").exists());
    }

    #[test]
    fn ensure_source_rejects_escaping_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("shadow");
        std::fs::write(&secret, b"root:x:0:0").unwrap();
        symlink(outside.path(), root.path().join("evil")).unwrap();

        // `ADD --from=stage /evil/shadow` resolves here.
        let resolved = root.path().join("evil/shadow");
        let err = ensure_source_contained(root.path(), &resolved).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn ensure_source_allows_internal_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::fs::write(root.path().join("real/file"), b"ok").unwrap();
        // An internal link whose target stays under the root is fine.
        symlink(root.path().join("real"), root.path().join("link")).unwrap();

        ensure_source_contained(root.path(), &root.path().join("link/file")).unwrap();
    }
}
