//! `umf doctor` — report which host runtimes UMF needs versus what's
//! installed, as sectioned **Container** / **VM** tables (NAME · PURPOSE ·
//! PATH · VERSION · STATUS), with an optional per-source build verdict.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use umf_builder::host_requirements::{
    DetectedRuntimes, ForwardPolicy, KvmStatus, NetworkEgress, SysctlState, compute_requirements,
    detect_all, detect_network_egress, verify_requirements, which_on_path,
};
use umf_core::architecture::Architecture;
use umf_core::ast::{Ast, FromSource};
use umf_oci::registry::ImageLayout;

use crate::cli::DoctorFormat;

/// Per-requirement health, driving the STATUS glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Present / satisfied.
    Ok,
    /// Present-but-degraded, or absent yet non-fatal (advisory).
    Warn,
    /// Absent / not found.
    Missing,
    /// Couldn't be determined.
    Unknown,
}

impl Status {
    /// Machine token for the JSON report.
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Missing => "missing",
            Status::Unknown => "unknown",
        }
    }

    /// The STATUS cell: a colored glyph on a TTY, a plain word otherwise (so
    /// piped / redirected output stays grep-able and free of escape codes).
    fn cell(self, color: bool) -> String {
        match (self, color) {
            (Status::Ok, true) => "\x1b[32m✓\x1b[0m".to_string(),
            (Status::Ok, false) => "ok".to_string(),
            (Status::Warn, true) => "\x1b[33m!\x1b[0m".to_string(),
            (Status::Warn, false) => "warn".to_string(),
            (Status::Missing, true) => "\x1b[31m✗\x1b[0m".to_string(),
            (Status::Missing, false) => "missing".to_string(),
            (Status::Unknown, true) => "\x1b[2m?\x1b[0m".to_string(),
            (Status::Unknown, false) => "?".to_string(),
        }
    }
}

/// One requirement row in a section table.
struct Row {
    name: String,
    purpose: String,
    path: String,
    version: String,
    status: Status,
}

impl Row {
    fn new(
        name: &str,
        purpose: &str,
        path: impl Into<String>,
        version: impl Into<String>,
        status: Status,
    ) -> Self {
        Self {
            name: name.to_string(),
            purpose: purpose.to_string(),
            path: path.into(),
            version: version.into(),
            status,
        }
    }
}

/// A `PATH`-resolved tool row: found ⇒ `Ok` with its path + probed version,
/// absent ⇒ `missing_status` (some tools are merely advisory when absent).
fn tool_row(name: &str, purpose: &str, found: Option<&Path>, missing_status: Status) -> Row {
    match found {
        Some(p) => Row::new(
            name,
            purpose,
            p.display().to_string(),
            probe_version(p).unwrap_or_default(),
            Status::Ok,
        ),
        None => Row::new(name, purpose, "<none on PATH>", "", missing_status),
    }
}

/// The recipe-scoped verdict, shown only when a path is passed.
struct Verdict {
    needs: Vec<(String, Status)>,
    blocked: bool,
    blocked_msg: Option<String>,
}

/// Print the host runtime report and, when `path` is given, a scoped
/// "this build needs …" verdict. Returns `FAILURE` on a parse/read error or a
/// blocked build verdict.
pub(crate) fn run_doctor(path: Option<&Path>, format: DoctorFormat) -> ExitCode {
    let detected = detect_all();
    let egress = detect_network_egress();
    let container = container_rows(&detected, &egress);
    let vm = vm_rows(&detected, &egress);

    let verdict = match path {
        Some(p) => match build_verdict(p, &detected) {
            Ok(v) => Some(v),
            // The error (parse / read / resolve) was already reported to stderr.
            Err(code) => return code,
        },
        None => None,
    };

    match format {
        DoctorFormat::Table => print_tables(&container, &vm, verdict.as_ref()),
        DoctorFormat::Json => print_json(&container, &vm, verdict.as_ref()),
    }

    match &verdict {
        Some(v) if v.blocked => {
            if let Some(msg) = &v.blocked_msg {
                eprintln!("{msg}");
            }
            ExitCode::FAILURE
        }
        _ => ExitCode::SUCCESS,
    }
}

// ── Row builders ─────────────────────────────────────────────────────────────

/// Container build + RUN-step requirements. The runtime itself is linked in;
/// the rest is the NAT'd-egress surface a `RUN` that hits the network needs.
fn container_rows(_detected: &DetectedRuntimes, net: &NetworkEgress) -> Vec<Row> {
    let mut rows = vec![Row::new(
        "container runtime",
        "RUN steps (libcontainer, no external CLI)",
        "built-in",
        env!("CARGO_PKG_VERSION"),
        Status::Ok,
    )];

    let (seccomp_detail, seccomp_status) = seccomp_status();
    rows.push(Row::new(
        "seccomp",
        "RUN syscall sandbox (deny-by-default)",
        "built-in",
        seccomp_detail,
        seccomp_status,
    ));

    // nft / ip_forward / FORWARD only matter for a RUN that reaches the network;
    // absent is advisory (Warn), not a hard Missing.
    rows.push(tool_row(
        "nft",
        "NAT masquerade for RUN-step egress",
        net.nft_path.as_deref(),
        Status::Warn,
    ));

    rows.push(Row::new(
        "net.ipv4.ip_forward",
        "route RUN egress out the host (UMF sets it per-build)",
        "/proc/sys/net/ipv4/ip_forward",
        "",
        match net.ip_forward {
            SysctlState::Enabled => Status::Ok,
            SysctlState::Disabled => Status::Warn,
            SysctlState::Unknown => Status::Unknown,
        },
    ));

    rows.push(Row::new(
        "FORWARD policy",
        "default-accept so NAT egress isn't dropped",
        "(nftables)",
        "",
        match net.forward_policy {
            ForwardPolicy::Accept => Status::Ok,
            ForwardPolicy::Drop => Status::Warn,
            ForwardPolicy::Unknown => Status::Unknown,
        },
    ));

    // Rootless builds: a rootless `umf build` enters one user namespace and
    // places RUN steps in a delegated systemd scope. These two are the real
    // prerequisites; absent is advisory (a rootful build needs neither).
    let (userns_detail, userns_status) = rootless_userns_status();
    rows.push(Row::new(
        "user namespaces",
        "unprivileged userns for rootless builds",
        "/proc/sys",
        userns_detail,
        userns_status,
    ));
    let (session_detail, session_status) = systemd_user_session_status();
    rows.push(Row::new(
        "systemd user session",
        "delegated cgroup v2 scope for rootless RUN steps",
        "(systemd)",
        session_detail,
        session_status,
    ));

    // Optional rootless helpers — the default rootless path needs neither.
    // fuse-overlayfs only backs the RUN overlay on kernels too old for an
    // unprivileged kernel-overlay mount; newuidmap/newgidmap only enable a
    // multi-id map (the default is single-id, written in-process).
    rows.push(tool_row(
        "fuse-overlayfs",
        "rootless RUN overlay fallback (kernels < 5.11)",
        which_on_path("fuse-overlayfs").as_deref(),
        Status::Warn,
    ));
    rows.push(tool_row(
        "newuidmap",
        "optional multi-id rootless map (default is single-id)",
        which_on_path("newuidmap").as_deref(),
        Status::Warn,
    ));
    rows.push(tool_row(
        "mkfs.erofs",
        "optional erofs layer-cache acceleration (falls back to unpack)",
        which_on_path("mkfs.erofs").as_deref(),
        Status::Warn,
    ));

    // Rootless egress: report the selected backend and, for the pasta backend,
    // whether the `pasta` binary is available. pasta is optional (native is the
    // default and needs no external binary); absent is advisory (Warn).
    let mode = umf_engine::rootless::egress_mode();
    rows.push(Row::new(
        "rootless egress backend",
        "network egress for rootless RUN steps (--rootless-net)",
        "",
        format!("{mode:?}").to_ascii_lowercase(),
        Status::Ok,
    ));
    rows.push(tool_row(
        "pasta",
        "optional rootless egress via passt/pasta helper (--rootless-net pasta)",
        which_on_path("pasta").as_deref(),
        Status::Warn,
    ));

    rows
}

/// Whether unprivileged user namespaces are usable for a rootless build:
/// `user.max_user_namespaces` must be non-zero, and (on Ubuntu 24.04+) the
/// AppArmor `apparmor_restrict_unprivileged_userns` policy must not block an
/// unconfined binary. Best-effort from `/proc/sys`; advisory for a rootful host.
fn rootless_userns_status() -> (String, Status) {
    let max = std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    if max == Some(0) {
        return (
            "user.max_user_namespaces=0 (unprivileged userns disabled)".to_string(),
            Status::Missing,
        );
    }
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
    if restricted {
        return (
            "apparmor_restrict_unprivileged_userns=1: grant umf a `userns,` profile, or unset it"
                .to_string(),
            Status::Warn,
        );
    }
    (
        "unprivileged user namespaces permitted".to_string(),
        Status::Ok,
    )
}

/// Whether a systemd *user* session with cgroup v2 is available — what a
/// rootless build needs to place RUN steps in a delegated scope. Best-effort:
/// the user session bus plus a cgroup v2 mount. Advisory for a rootful host.
fn systemd_user_session_status() -> (String, Status) {
    let cgroup_v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    let user_bus = std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|d| Path::new(&d).join("bus").exists())
        || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    match (cgroup_v2, user_bus) {
        (true, true) => (
            "cgroup v2 + user session bus present".to_string(),
            Status::Ok,
        ),
        (true, false) => (
            "no systemd user session bus (rootless cgroups need one; `loginctl enable-linger`)"
                .to_string(),
            Status::Warn,
        ),
        (false, _) => (
            "cgroup v2 not mounted (rootless cgroups need it)".to_string(),
            Status::Warn,
        ),
    }
}

/// VM / bootable requirements: the VMM backends, KVM, firmware, and the
/// cloud-hypervisor port-forward DHCP.
fn vm_rows(detected: &DetectedRuntimes, net: &NetworkEgress) -> Vec<Row> {
    let qemu_name = Architecture::host().qemu_binary_name();
    let mut rows = vec![tool_row(
        qemu_name,
        "VM RUN backend + `umf run` (default VMM)",
        detected.qemu_path.as_deref(),
        Status::Missing,
    )];

    // cloud-hypervisor is the *alternative* VMM; absent is advisory, not Missing.
    rows.push(tool_row(
        "cloud-hypervisor",
        "alternative VMM (`umf run --vmm ch`)",
        detected.cloud_hypervisor_path.as_deref(),
        Status::Warn,
    ));

    let (kvm_purpose, kvm_status) = match detected.kvm_status {
        KvmStatus::Accessible => ("hardware acceleration for VMs", Status::Ok),
        KvmStatus::PresentNoPermission => (
            "present but no permission — add your user to the `kvm` group",
            Status::Warn,
        ),
        KvmStatus::Absent => (
            "absent — VMs fall back to slow TCG software emulation",
            Status::Warn,
        ),
    };
    rows.push(Row::new(
        "/dev/kvm",
        kvm_purpose,
        "/dev/kvm",
        "",
        kvm_status,
    ));

    rows.push(match crate::cli::run::host_uefi_firmware() {
        Some(p) => Row::new(
            "UEFI firmware",
            "OVMF / AAVMF to boot compiled disks (`umf run`)",
            p.display().to_string(),
            "",
            Status::Ok,
        ),
        None => Row::new(
            "UEFI firmware",
            "OVMF / AAVMF to boot compiled disks (`umf run`)",
            "<none at usual host paths>",
            "",
            Status::Missing,
        ),
    });

    // dnsmasq is the default in-VM DHCP for the cloud-hypervisor port-forward
    // path, so it belongs with the VM section (an operator can swap it via
    // `--dhcp-command`); absent is advisory.
    rows.push(tool_row(
        "dnsmasq",
        "default in-VM DHCP for `--vmm ch` port-forward",
        net.dnsmasq_path.as_deref(),
        Status::Warn,
    ));

    // ukify assembles a UKI for `flavor=uki` compiles and has *no* fallback (a
    // UKI is the only bootloader-less boot path), so its absence hard-fails such
    // a compile; advisory here since other flavors don't need it.
    rows.push(tool_row(
        "ukify",
        "flavor=uki UKI assembly (`umf compile`; no fallback)",
        which_on_path("ukify").as_deref(),
        Status::Warn,
    ));

    rows
}

/// Best-effort seccomp profile health, plus a short detail (its syscall count)
/// for the VERSION column. The default profile is compiled into the binary and
/// applied by libcontainer; a load failure is a clear doctor finding rather
/// than a surprise at the first RUN.
fn seccomp_status() -> (String, Status) {
    match umf_engine::seccomp::default_profile() {
        Ok(profile) => {
            let allowed: usize = profile
                .syscalls()
                .as_ref()
                .map(|blocks| blocks.iter().map(|b| b.names().len()).sum())
                .unwrap_or(0);
            (format!("{allowed} syscalls"), Status::Ok)
        }
        Err(_) => (String::new(), Status::Missing),
    }
}

// ── Version probing ──────────────────────────────────────────────────────────

/// Best-effort version string for a tool: run `<bin> --version` and extract the
/// first version-shaped token. Failure-tolerant — a spawn error, non-zero exit,
/// or unparseable output yields `None` (a blank VERSION cell), never an error.
fn probe_version(bin: &Path) -> Option<String> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()?;
    // Most tools print to stdout; a few (older utilities) use stderr.
    let bytes = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let text = String::from_utf8_lossy(&bytes);
    extract_version_token(text.lines().next().unwrap_or(""))
}

/// Pull a `v?<digits>.<...>` token out of a `--version` first line, e.g.
/// `QEMU emulator version 8.2.2 (Debian…)` ⇒ `8.2.2`,
/// `cloud-hypervisor v37.0.0` ⇒ `v37.0.0`, `Dnsmasq version 2.90 …` ⇒ `2.90`.
fn extract_version_token(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|tok| {
            let t = tok.trim_start_matches(['v', 'V']);
            t.contains('.') && t.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .map(str::to_string)
}

// ── Table rendering ──────────────────────────────────────────────────────────

fn print_tables(container: &[Row], vm: &[Row], verdict: Option<&Verdict>) {
    // Color only on a real terminal, and never when NO_COLOR is set
    // (https://no-color.org/).
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    print!(
        "{}",
        render_section("Container build & RUN", container, color)
    );
    println!();
    print!("{}", render_section("VM / bootable", vm, color));
    if let Some(v) = verdict {
        println!();
        print!("{}", render_verdict(v));
    }
}

/// Render one section as an aligned table. The four text columns are padded to
/// their content width; STATUS is last and unpadded, so its (possibly
/// ANSI-colored) glyph never throws off the alignment of the columns before it.
fn render_section(title: &str, rows: &[Row], color: bool) -> String {
    let headers = ["NAME", "PURPOSE", "PATH", "VERSION"];
    let mut w = [0usize; 4];
    for (i, h) in headers.iter().enumerate() {
        w[i] = h.chars().count();
    }
    for r in rows {
        w[0] = w[0].max(r.name.chars().count());
        w[1] = w[1].max(r.purpose.chars().count());
        w[2] = w[2].max(r.path.chars().count());
        w[3] = w[3].max(r.version.chars().count());
    }
    let pad = |s: &str, width: usize| format!("{s:<width$}");

    let mut out = String::new();
    if color {
        out.push_str(&format!("\x1b[1m{title}\x1b[0m\n"));
    } else {
        out.push_str(&format!("{title}\n"));
    }
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}\n",
        pad(headers[0], w[0]),
        pad(headers[1], w[1]),
        pad(headers[2], w[2]),
        pad(headers[3], w[3]),
        "STATUS",
    ));
    for r in rows {
        out.push_str(&format!(
            "  {}  {}  {}  {}  {}\n",
            pad(&r.name, w[0]),
            pad(&r.purpose, w[1]),
            pad(&r.path, w[2]),
            pad(&r.version, w[3]),
            r.status.cell(color),
        ));
    }
    out
}

fn render_verdict(v: &Verdict) -> String {
    let mut out = String::from("This build needs:\n");
    if v.needs.is_empty() {
        out.push_str("  - nothing! (pure-Rust pipeline + umf-engine cover this build)\n");
    } else {
        for (name, st) in &v.needs {
            let word = match st {
                Status::Ok => "found",
                Status::Warn => "degraded",
                Status::Missing => "MISSING",
                Status::Unknown => "unknown",
            };
            out.push_str(&format!("  - {name}: {word}\n"));
        }
    }
    out.push_str(&format!(
        "Status: {}\n",
        if v.blocked {
            "blocked"
        } else {
            "ready to build."
        }
    ));
    out
}

fn print_json(container: &[Row], vm: &[Row], verdict: Option<&Verdict>) {
    let rows_json = |rows: &[Row]| -> Vec<serde_json::Value> {
        rows.iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "purpose": r.purpose,
                    "path": r.path,
                    "version": r.version,
                    "status": r.status.as_str(),
                })
            })
            .collect()
    };
    let mut obj = serde_json::Map::new();
    obj.insert("container".into(), rows_json(container).into());
    obj.insert("vm".into(), rows_json(vm).into());
    if let Some(v) = verdict {
        let needs: Vec<serde_json::Value> = v
            .needs
            .iter()
            .map(|(name, st)| serde_json::json!({ "name": name, "status": st.as_str() }))
            .collect();
        obj.insert(
            "build".into(),
            serde_json::json!({
                "needs": needs,
                "status": if v.blocked { "blocked" } else { "ready" },
            }),
        );
    }
    let value = serde_json::Value::Object(obj);
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    );
}

// ── Recipe-scoped verdict ────────────────────────────────────────────────────

/// Build the per-recipe verdict. Returns `Err(FAILURE)` (after reporting to
/// stderr) when the recipe can't be resolved / read / parsed.
fn build_verdict(path: &Path, detected: &DetectedRuntimes) -> Result<Verdict, ExitCode> {
    let recipe = match crate::cli::util::resolve_recipe(Some(path), None) {
        Ok(r) => r.recipe,
        Err(err) => {
            eprintln!("error: {err}");
            return Err(ExitCode::FAILURE);
        }
    };
    let source = match std::fs::read_to_string(&recipe) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("error: cannot read {}: {err}", recipe.display());
            return Err(ExitCode::FAILURE);
        }
    };
    let ast = match umf_parser::parse(&source) {
        Ok(ast) => ast,
        Err(diagnostics) => {
            let source_name = recipe.display().to_string();
            let mut stderr = std::io::stderr().lock();
            let _ = umf_parser::diagnostics::report_all(
                &diagnostics,
                &mut stderr,
                &source_name,
                &source,
            );
            return Err(ExitCode::FAILURE);
        }
    };

    let required = compute_requirements(&ast, probe_bootable_cached(&ast));
    let mut needs = Vec::new();
    if required.qemu {
        let st = if detected.qemu_path.is_some() {
            Status::Ok
        } else {
            Status::Missing
        };
        needs.push((Architecture::host().qemu_binary_name().to_string(), st));
    }
    if required.kvm {
        let st = match detected.kvm_status {
            KvmStatus::Accessible => Status::Ok,
            KvmStatus::PresentNoPermission | KvmStatus::Absent => Status::Warn,
        };
        needs.push(("/dev/kvm".to_string(), st));
    }
    let (blocked, blocked_msg) = match verify_requirements(&required) {
        Ok(_) => (false, None),
        Err(err) => (true, Some(err.to_string())),
    };
    Ok(Verdict {
        needs,
        blocked,
        blocked_msg,
    })
}

/// Best-effort, cache-only bootable check for the doctor verdict: a `FROM`
/// resolving to a cached `type=kernel` artifact. doctor does no network I/O, so
/// an un-pulled kernel reads as "not bootable" here — the verdict is advisory,
/// and `umf build` resolves the target authoritatively.
fn probe_bootable_cached(ast: &Ast) -> bool {
    let Some(stage) = ast.stages.first() else {
        return false;
    };
    let FromSource::Reference(r) = &stage.from.source else {
        return false;
    };
    let Ok(layout_dir) = crate::cli::util::default_layout_dir() else {
        return false;
    };
    let Ok(layout) = ImageLayout::init(&layout_dir) else {
        return false;
    };
    umf_builder::introspect::introspect(&layout, r.value.as_str())
        .map(|p| p.kind.is_kernel())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
