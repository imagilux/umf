//! Rootless RUN-step network egress: backend selection plus the `pasta` backend.
//!
//! Built on [`crate::OwnedNetns`] (the netns we create, own, and the container
//! joins). The egress backend then configures that namespace for outbound
//! traffic. Selection is [`EgressMode`]:
//!
//! - [`EgressMode::Native`] — the in-process userspace stack ([`gateway`] +
//!   [`tapdev`], wired up in [`native`]): a `smoltcp` `any_ip` transparent
//!   gateway over a tap fd that re-originates the container's traffic from
//!   ordinary host sockets, with no external binary. **The default**: the
//!   sovereign egress backend, working air-gapped.
//! - [`EgressMode::None`] — loopback only, no egress (the namespace already has
//!   `lo` up).
//! - [`EgressMode::Pasta`] — userspace egress via the external `pasta` helper
//!   (the one deliberate external-binary opt-in). `pasta` attaches to our
//!   namespace, adds a tap with addresses/routes/DNS, and re-originates the
//!   container's traffic from the host netns over ordinary sockets — the only
//!   way across the userns→host boundary without privilege.

pub mod gateway;
mod native;
pub mod tapdev;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

pub use gateway::{Gateway, GatewayStats};
pub use tapdev::TapDevice;

use crate::NetError;
use crate::owned_netns::OwnedNetns;

/// How a rootless RUN step reaches the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EgressMode {
    /// Loopback only — no egress.
    None,
    /// Userspace egress via the external `pasta` helper (opt-in).
    Pasta,
    /// In-process userspace stack: a `smoltcp` `any_ip` gateway over a tap in
    /// the netns UMF owns — no external binary, works air-gapped. The
    /// default: the sovereign egress backend, on out of the box.
    #[default]
    Native,
}

impl EgressMode {
    /// Resolve from the `UMF_ROOTLESS_NET` environment variable, defaulting to
    /// [`EgressMode::Native`] when unset or unrecognised.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("UMF_ROOTLESS_NET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_default()
    }
}

impl FromStr for EgressMode {
    type Err = ParseEgressModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "pasta" => Ok(Self::Pasta),
            "native" => Ok(Self::Native),
            other => Err(ParseEgressModeError(other.to_string())),
        }
    }
}

/// Error parsing an [`EgressMode`] from a string.
#[derive(Debug, thiserror::Error)]
#[error("unknown rootless egress mode `{0}` (expected `none`, `pasta`, or `native`)")]
pub struct ParseEgressModeError(String);

/// A rootless RUN step's network: the namespace we own (loopback up) plus the
/// selected egress backend. Held for the container's lifetime; on drop the
/// egress backend is torn down first, then the namespace is released.
#[derive(Debug)]
pub struct RootlessNet {
    // The egress backend, declared before `netns` so it is dropped — and
    // signalled to exit (SIGTERM the pasta daemon / stop the native gateway
    // thread) — before the namespace it ran in is released; field drop order is
    // declaration order. At most one is `Some` (per [`EgressMode`]). Both are
    // underscore-named: held only so their `Drop` runs at teardown, never read.
    _pasta: Option<PastaEgress>,
    _native: Option<native::NativeEgress>,
    netns: OwnedNetns,
}

impl RootlessNet {
    /// Create the owned netns and set up the selected egress backend in it,
    /// enforcing `policy` (the SSRF deny set). Only the native backend enforces
    /// `policy` precisely, at connect time; `pasta` relies on its coarser
    /// baseline guards (`--no-map-gw`); `none` has no egress to police.
    ///
    /// # Errors
    /// [`NetError`] if the namespace or the egress backend fails to set up.
    pub fn setup(mode: EgressMode, policy: crate::ssrf::EgressPolicy) -> Result<Self, NetError> {
        let netns = OwnedNetns::create()?;
        let (pasta, native) = match mode {
            EgressMode::None => (None, None),
            EgressMode::Pasta => (Some(PastaEgress::start(netns.spec_path())?), None),
            EgressMode::Native => (None, Some(native::NativeEgress::start(&netns, policy)?)),
        };
        Ok(Self {
            _pasta: pasta,
            _native: native,
            netns,
        })
    }

    /// The container's network-namespace `path` for the OCI runtime spec.
    #[must_use]
    pub fn spec_path(&self) -> &Path {
        self.netns.spec_path()
    }
}

/// The `pasta` userspace egress helper attached to our namespace.
///
/// `pasta` configures a tap with addresses, routes, and DNS forwarding inside
/// the namespace and re-originates the container's traffic from the host netns
/// over ordinary sockets. It is spawned in its default **background** mode: the
/// foreground process exits only after setup is complete and the daemon has
/// detached, so a successful `status()` means the namespace is ready before the
/// container starts. The detached daemon's PID (from `--pid`) is signalled on
/// drop.
#[derive(Debug)]
struct PastaEgress {
    pid: i32,
    pidfile: PathBuf,
}

impl PastaEgress {
    fn start(netns_pin: &Path) -> Result<Self, NetError> {
        let pidfile = pasta_pid_path();
        let status = Command::new("pasta")
            .arg("--quiet")
            // Configure the tap's address + default route inside the namespace
            // directly, rather than expecting the container to run a DHCP client
            // (a build RUN step won't). Without this the interface comes up
            // unaddressed and egress is dead.
            .arg("--config-net")
            // Don't map the gateway address to the host: a baseline guard so the
            // container can't reach the host's loopback services through the
            // gateway IP. The full default-deny SSRF policy (incl. cloud
            // metadata / RFC1918) is a follow-up.
            .arg("--no-map-gw")
            .arg("--netns")
            .arg(netns_pin)
            .arg("--pid")
            .arg(&pidfile)
            .status()
            .map_err(|e| {
                NetError::Runtime(format!(
                    "spawning pasta failed (is the `passt`/`pasta` package installed?): {e}"
                ))
            })?;
        if !status.success() {
            let _ = std::fs::remove_file(&pidfile);
            return Err(NetError::Runtime(format!(
                "pasta exited with {status} setting up the netns"
            )));
        }
        let pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .ok_or_else(|| NetError::Runtime("pasta did not write its pid".to_string()))?;
        Ok(Self { pid, pidfile })
    }
}

impl Drop for PastaEgress {
    fn drop(&mut self) {
        // Best-effort: SIGTERM the detached pasta daemon (pid from its `--pid`
        // file) and remove the pid file.
        let _ = kill(Pid::from_raw(self.pid), Signal::SIGTERM);
        let _ = std::fs::remove_file(&self.pidfile);
    }
}

static PASTA_SEQ: AtomicU64 = AtomicU64::new(0);

/// A per-process-unique path in the user-private `XDG_RUNTIME_DIR` (fallback:
/// the temp dir) for pasta to write its pid.
fn pasta_pid_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let seq = PASTA_SEQ.fetch_add(1, Ordering::Relaxed);
    base.join(format!("umf-pasta.{}.{seq}.pid", std::process::id()))
}

#[cfg(test)]
mod tests;
