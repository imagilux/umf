//! Runtime-config emission for a bootable build — currently the `EXPOSE`
//! firewall.
//!
//! `EXPOSE` carries a real in-image effect, not just metadata: it generates an
//! actual default-deny nftables ruleset (`/etc/nftables.conf` plus
//! `/etc/nftables.d/`) and enables `nftables.service` so the ruleset loads at
//! boot. The emission target is the staging tree (which the disk-projection
//! step then packs into the on-disk ROOTFS partition).
//!
//! Init-system selection comes from the `ENTRYPOINT` directive and picks the
//! service-enable convention for `nftables.service`:
//!
//! * `systemd` (default) — `/etc/systemd/system/multi-user.target.wants/`.
//! * `openrc` — `/etc/runlevels/default/`.
//! * binary path / exec form (appliance) — no init, so the service is not
//!   enabled (the appliance binary is PID 1).
//! * `none` — invalid for a bootable build (the kernel needs something to exec
//!   as PID 1).

use std::fs;
use std::io::Write as _;
use std::path::Path;

use thiserror::Error;
use tracing::{debug, info};
use umf_core::ast::{Directive, EntrypointInit, ExposeProtocol, Stage};
use umf_core::types::{ServiceUnitName, UnitSuffix};

use umf_oci::staging::BuildStaging;

// ── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by [`apply_runtime_config`].
#[derive(Debug, Error)]
pub enum RuntimeConfigError {
    /// `ENTRYPOINT none` was declared on a bootable build — the kernel needs
    /// an init binary, even a stub one. The container target accepts `none`
    /// (the runtime supplies PID 1); a bootable build doesn't.
    #[error("ENTRYPOINT none is not valid for a bootable build (no PID 1)")]
    EntrypointNone,

    /// `EXPOSE` was declared but the produced image can't enforce the
    /// default-deny firewall it promises — the userland ships no `nft` binary,
    /// or the `ENTRYPOINT` is not an init system that loads the ruleset at boot.
    /// Failing closed: silently shipping an image whose EXPOSE default-deny
    /// never loads would be a firewall that isn't there (the spec is explicit
    /// that EXPOSE is default-deny, not metadata).
    #[error("EXPOSE cannot be enforced for this image: {detail}")]
    ExposeUnenforceable {
        /// What's missing and how to fix it.
        detail: String,
    },

    /// Underlying I/O error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// In-image locations the generated `nftables.conf` / init unit expect the
/// `nft` binary at. Its presence is the build-time proxy for "the userland
/// ships the nftables package" (which also carries the init unit that loads the
/// ruleset).
const NFT_BINARY_PATHS: &[&str] = &["usr/sbin/nft", "sbin/nft", "usr/bin/nft", "bin/nft"];

fn nft_binary_present(root: &Path) -> bool {
    NFT_BINARY_PATHS.iter().any(|rel| root.join(rel).exists())
}

// ── Result ──────────────────────────────────────────────────────────────────

/// Summary of what was written into staging.
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfigReport {
    /// Init system selected (per `ENTRYPOINT`).
    pub init_system: Option<InitSystemKind>,
    /// Number of `EXPOSE` directives that became nftables ACCEPT rules.
    pub exposed_ports: usize,
}

/// Init system the directives target — derived from `ENTRYPOINT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystemKind {
    /// `ENTRYPOINT systemd` (or absent — spec default).
    Systemd,
    /// `ENTRYPOINT openrc`.
    OpenRc,
    /// Binary-form ENTRYPOINT (appliance): no init; service enable/disable
    /// is a no-op.
    Binary,
}

// ── Public entry ────────────────────────────────────────────────────────────

/// Apply the runtime-config directives in `stage` to the `staging` tree.
///
/// Currently that is `EXPOSE`: collect every exposed port into one default-deny
/// nftables ruleset and enable `nftables.service` for the stage's init system.
/// Idempotent, and a no-op when the stage declares no `EXPOSE`.
pub fn apply_runtime_config(
    stage: &Stage,
    staging: &mut BuildStaging,
) -> Result<RuntimeConfigReport, RuntimeConfigError> {
    let init_system = pick_init_system(stage)?;
    let mut report = RuntimeConfigReport {
        init_system,
        ..RuntimeConfigReport::default()
    };

    let root = staging.path().to_path_buf();
    info!(?init_system, "runtime config: applying directives");

    // Firewall: collect all EXPOSE directives, emit one nftables file.
    let exposes: Vec<(u16, ExposeProtocol)> = stage
        .directives
        .iter()
        .filter_map(|d| {
            if let Directive::Expose(e) = d {
                Some((e.port, e.protocol))
            } else {
                None
            }
        })
        .collect();
    if !exposes.is_empty() {
        // Fail closed: EXPOSE promises a default-deny firewall. If nothing in
        // the produced image would load it, refuse to build rather than ship an
        // image that claims default-deny but has no firewall at all.

        // 1. Only an init-system ENTRYPOINT (systemd/openrc) runs the service
        //    that loads the ruleset at boot. A binary/appliance ENTRYPOINT has
        //    no service manager, so the ruleset would sit unloaded.
        if !matches!(
            init_system,
            Some(InitSystemKind::Systemd | InitSystemKind::OpenRc)
        ) {
            return Err(RuntimeConfigError::ExposeUnenforceable {
                detail: "the ENTRYPOINT is a binary/appliance, so no init system loads the \
                         nftables ruleset at boot; EXPOSE default-deny requires an init-system \
                         ENTRYPOINT (systemd or openrc)"
                    .to_string(),
            });
        }

        // 2. The generated ruleset is loaded by the userland's nftables package;
        //    without an `nft` binary the service can't apply it (dangling
        //    enable) and the firewall silently never comes up.
        if !nft_binary_present(&root) {
            return Err(RuntimeConfigError::ExposeUnenforceable {
                detail: "the userland ships no `nft` binary (looked under /usr/sbin, /sbin, \
                         /usr/bin, /bin), so the enabled nftables service can't load the \
                         default-deny ruleset; add an nftables package to the userland (e.g. \
                         `RUN apk add nftables`) or drop the EXPOSE directive"
                    .to_string(),
            });
        }

        write_nftables(&root, &exposes)?;
        debug!(count = exposes.len(), "wrote nftables EXPOSE ruleset");
        report.exposed_ports = exposes.len();
        // Hardcoded valid name; the parser would catch any typo at compile time
        // of the builder, not at runtime.
        #[allow(clippy::expect_used)]
        let nftables =
            ServiceUnitName::new("nftables").expect("`nftables` is a valid bare unit name");
        enable_service(&root, init_system, &nftables)?;
    }

    Ok(report)
}

// ── Internals ───────────────────────────────────────────────────────────────

fn pick_init_system(stage: &Stage) -> Result<Option<InitSystemKind>, RuntimeConfigError> {
    for directive in &stage.directives {
        if let Directive::Entrypoint(ep) = directive {
            return Ok(Some(match &ep.init {
                EntrypointInit::Systemd => InitSystemKind::Systemd,
                EntrypointInit::OpenRc => InitSystemKind::OpenRc,
                EntrypointInit::Path(_) | EntrypointInit::Exec(_) => InitSystemKind::Binary,
                EntrypointInit::None => return Err(RuntimeConfigError::EntrypointNone),
            }));
        }
    }
    // Spec default for a bootable build.
    Ok(Some(InitSystemKind::Systemd))
}

fn write_nftables(
    root: &Path,
    exposes: &[(u16, ExposeProtocol)],
) -> Result<(), RuntimeConfigError> {
    let nft_dir = root.join("etc").join("nftables.d");
    fs::create_dir_all(&nft_dir)?;
    let mut body = String::new();
    body.push_str("# Generated by UMF builder — UMF EXPOSE directives.\n");
    body.push_str("# Default-deny inbound; only explicitly-exposed ports are reachable.\n");
    body.push_str("table inet umf {\n");
    body.push_str("    chain input {\n");
    body.push_str("        type filter hook input priority filter; policy drop;\n");
    // Always allow loopback + already-established connections.
    body.push_str("        iif lo accept\n");
    body.push_str("        ct state established,related accept\n");
    body.push_str("        ip protocol icmp accept\n");
    body.push_str("        ip6 nexthdr ipv6-icmp accept\n");
    for (port, proto) in exposes {
        let proto_str = match proto {
            ExposeProtocol::Tcp => "tcp",
            ExposeProtocol::Udp => "udp",
        };
        body.push_str(&format!("        {proto_str} dport {port} accept\n",));
    }
    body.push_str("    }\n");
    body.push_str("}\n");

    let mut f = fs::File::create(nft_dir.join("umf-expose.nft"))?;
    f.write_all(body.as_bytes())?;

    // The stock `nftables.service` loads `/etc/nftables.conf`, NOT the
    // `/etc/nftables.d/` drop-in directory — so the fragment above would never
    // be applied on its own and the spec's "EXPOSE = default-deny" guarantee
    // would silently not hold for the produced image. Write a top-level
    // config that flushes and includes the drop-in so the ruleset actually
    // loads at boot. We own this file: for a UMF image that declares EXPOSE the
    // default-deny posture is the explicit intent.
    let conf = "#!/usr/sbin/nft -f\n\
                # Generated by UMF builder. Loads the UMF EXPOSE default-deny ruleset.\n\
                flush ruleset\n\
                include \"/etc/nftables.d/*.nft\"\n";
    let mut cf = fs::File::create(root.join("etc").join("nftables.conf"))?;
    cf.write_all(conf.as_bytes())?;
    Ok(())
}

/// Enable a service in the init system. Returns `true` when something
/// was written, `false` when the init system doesn't have an enable
/// concept (`Binary`).
fn enable_service(
    root: &Path,
    init_system: Option<InitSystemKind>,
    service: &ServiceUnitName,
) -> Result<bool, RuntimeConfigError> {
    use crate::runtime_writer::{InitSystem as RwInit, write_enable_link};
    let (unit, init) = match init_system {
        Some(InitSystemKind::Systemd) => (
            service.with_default_suffix(UnitSuffix::Service),
            RwInit::Systemd,
        ),
        Some(InitSystemKind::OpenRc) => (service.bare_name().to_string(), RwInit::OpenRc),
        Some(InitSystemKind::Binary) | None => return Ok(false),
    };
    write_enable_link(root, init, &unit)?;
    Ok(true)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
