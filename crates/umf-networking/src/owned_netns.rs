//! A network namespace UMF creates and owns, for rootless container egress.
//!
//! A rootless build cannot reach *into* youki's container netns from outside:
//! youki's init is non-dumpable, so its `/proc/<pid>/ns/net` is owned by global
//! root and an unprivileged build gets `EACCES` trying to `setns` in. So
//! we invert ownership — we create the netns, configure it (loopback up, and
//! later a tap for egress), and the container **joins it** via the OCI spec's
//! network-namespace `path`. This is the rootless analogue of what
//! [`crate::vmnet`] does for VMs.
//!
//! The namespace is kept alive by an owning fd (the unsharing thread can exit;
//! the fd pins it, same as `vmnet::create_netns`). To hand it to youki we
//! **bind-mount it to a regular file** (the `ip netns` technique): youki's init
//! is forked from us and inherits our mount namespace, so it opens that plain
//! file and `setns` into the namespace we own. A bind-mount path is used rather
//! than `/proc/<our-pid>/fd/<n>` because youki's init cannot open our fd table
//! across processes (it lacks ptrace access — that path gives `EACCES`).
//! Dropping [`OwnedNetns`] unmounts + removes the file and closes the fd,
//! releasing the namespace once the container in it has also exited.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::mount::{MntFlags, MsFlags, mount, umount2};

use crate::NetError;

/// A network namespace we created and own, with loopback up, bind-mounted to a
/// path a container can join via [`OwnedNetns::spec_path`].
#[derive(Debug)]
pub struct OwnedNetns {
    /// Owning fd to the (unnamed) namespace. Keeps it alive with no process in
    /// it; dropped last on teardown.
    netns: OwnedFd,
    /// The bind-mount path handed to youki. Unmounted + removed on drop.
    pin: PathBuf,
}

impl OwnedNetns {
    /// Create a network namespace, bring its loopback interface up, bind-mount
    /// it to a path, and return a guard that owns it. The caller sets
    /// [`Self::spec_path`] as the container's network-namespace path and holds
    /// the guard for the container's lifetime.
    ///
    /// # Errors
    /// [`NetError`] if the namespace can't be created, `lo` can't be raised, or
    /// the bind-mount fails.
    pub fn create() -> Result<Self, NetError> {
        let netns = create_netns_with_loopback()?;
        let pin = pin_path(netns.as_raw_fd());
        // On a bind-mount failure `netns` drops here, releasing the namespace
        // (and `bind_mount_netns` already removes the pin file it created).
        bind_mount_netns(netns.as_raw_fd(), &pin)?;
        Ok(Self { netns, pin })
    }

    /// The path to set as the container's network-namespace `path` in the OCI
    /// runtime spec. youki opens it and `setns` into the namespace we own.
    #[must_use]
    pub fn spec_path(&self) -> &Path {
        &self.pin
    }

    /// Raw fd of the namespace, for in-process configuration (e.g. adding a tap
    /// for egress) that `setns`-es in by fd. Valid while the guard is alive.
    #[must_use]
    pub fn raw_fd(&self) -> RawFd {
        self.netns.as_raw_fd()
    }
}

impl Drop for OwnedNetns {
    fn drop(&mut self) {
        // Best-effort: lazy-unmount the bind mount and remove the pin file, then
        // the owning fd drops, releasing the namespace.
        let _ = umount2(&self.pin, MntFlags::MNT_DETACH);
        let _ = std::fs::remove_file(&self.pin);
    }
}

/// Monotonic per-process counter so two namespaces never reuse a pin name (raw
/// fds are recycled over a process's life).
static PIN_SEQ: AtomicU64 = AtomicU64::new(0);

/// The bind-mount target for a namespace fd. Placed in the **user-private**
/// `XDG_RUNTIME_DIR` (systemd creates it mode 0700, owned by the user, so a
/// co-tenant cannot plant a symlink there), falling back to the temp dir. The
/// name is unique per process (pid + fd + counter), and the file is created
/// with `O_EXCL | O_NOFOLLOW` (see [`bind_mount_netns`]), so even on the
/// world-writable fallback a pre-existing path or symlink is refused rather
/// than followed.
fn pin_path(fd: RawFd) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let seq = PIN_SEQ.fetch_add(1, Ordering::Relaxed);
    base.join(format!("umf-netns.{}.{fd}.{seq}", std::process::id()))
}

/// Bind-mount the namespace (referred to by `fd`) onto `pin`. The pin file is
/// created with `create_new` (`O_CREAT | O_EXCL`) plus `O_NOFOLLOW`, so a
/// symlink or pre-existing file at the path is refused, not followed — closing
/// the symlink/TOCTOU redirect of the create or the subsequent bind-mount. The
/// source `/proc/self/fd/<fd>` resolves to the namespace inode we hold;
/// bind-mounting it pins the namespace at a plain path.
fn bind_mount_netns(fd: RawFd, pin: &Path) -> Result<(), NetError> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(pin)
        .map_err(|e| NetError::VmNet(format!("create netns pin {}: {e}", pin.display())))?;
    let src = format!("/proc/self/fd/{fd}");
    if let Err(e) = mount(
        Some(src.as_str()),
        pin,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    ) {
        let _ = std::fs::remove_file(pin);
        return Err(NetError::VmNet(format!(
            "bind-mount netns {src} -> {}: {e}",
            pin.display()
        )));
    }
    Ok(())
}

/// Create a network namespace and raise its loopback, returning the owning fd.
///
/// `unshare(CLONE_NEWNET)` moves only the calling thread, so we do it (and the
/// loopback-up, which must run *inside* the new namespace) on a dedicated
/// thread, then capture `/proc/thread-self/ns/net` as an [`OwnedFd`] before the
/// thread exits. `/proc/thread-self` (not `/proc/self`) is the unsharing task,
/// not the thread-group leader. Same mechanism as `vmnet::create_netns`, plus
/// the loopback step.
fn create_netns_with_loopback() -> Result<OwnedFd, NetError> {
    std::thread::Builder::new()
        .name("umf-owned-netns".to_string())
        .spawn(|| -> Result<OwnedFd, NetError> {
            nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNET)
                .map_err(|e| NetError::VmNet(format!("unshare(CLONE_NEWNET): {e}")))?;

            // We are now in the new namespace; raise loopback via in-process
            // rtnetlink (a fresh netlink socket here lands in this namespace).
            let rt = crate::current_thread_rt()?;
            rt.block_on(async {
                let (conn, handle, _) = rtnetlink::new_connection()?;
                let _conn = tokio::spawn(conn);
                let lo = crate::link_index(&handle, "lo").await?;
                handle
                    .link()
                    .set(lo)
                    .up()
                    .execute()
                    .await
                    .map_err(crate::nl)?;
                Ok::<(), NetError>(())
            })?;

            let f = std::fs::File::open("/proc/thread-self/ns/net")
                .map_err(|e| NetError::VmNet(format!("open /proc/thread-self/ns/net: {e}")))?;
            Ok(OwnedFd::from(f))
        })
        .map_err(|e| NetError::Runtime(format!("spawning owned-netns thread: {e}")))?
        .join()
        .map_err(|_| NetError::Runtime("owned-netns thread panicked".to_string()))?
}
