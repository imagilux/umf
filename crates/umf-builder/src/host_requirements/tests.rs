//! Unit tests for the `host_requirements` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use umf_parser::parse;

#[test]
fn container_build_needs_no_host_runtime() {
    // The container target is fully in-process via
    // umf-engine + libcontainer — there's nothing to detect on
    // the host PATH for it.
    let ast = parse("FROM debian:bookworm\nRUN echo hi\n").expect("parse");
    let req = compute_requirements(&ast, false);
    assert!(!req.qemu);
    assert!(!req.kvm);
}

#[test]
fn vm_build_with_no_run_needs_no_runtime() {
    let ast = parse("FROM imagilux/kernel-linux:7.0\n").expect("parse");
    let req = compute_requirements(&ast, true);
    assert!(!req.qemu);
    assert!(!req.kvm);
}

#[test]
fn vm_build_with_run_needs_qemu_and_kvm() {
    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         ADD alpine:3.21.0 /\n\
         ENTRYPOINT systemd\n\
         RUN apk add curl\n",
    )
    .expect("parse");
    let req = compute_requirements(&ast, true);
    assert!(req.qemu);
    assert!(req.kvm);
}

#[test]
fn multistage_container_build_needs_no_host_runtime() {
    let ast = parse(
        "\
FROM debian:bookworm AS builder
RUN make

FROM debian:bookworm-slim
ADD --from=builder /work/output /usr/local/bin/output
",
    )
    .expect("parse");
    let req = compute_requirements(&ast, false);
    assert!(!req.qemu);
    assert!(!req.kvm);
}

/// Even when nothing is required, `verify_requirements` still
/// reports what the host has — that's what `umf doctor` reads.
#[test]
fn empty_requirements_yields_ok_with_detected_snapshot() {
    let req = RequiredRuntimes::default();
    let detected = verify_requirements(&req).expect("ok");
    // Detected is whatever the host carries — we just confirm the
    // call shape works.
    let _ = detected.qemu_path;
    let _ = detected.kvm_status;
}

/// Asking for a runtime that isn't present produces an error whose
/// `Display` names the missing runtime + a per-architecture hint.
#[test]
fn missing_runtime_error_displays_helpfully() {
    let err = MissingRuntimeError {
        missing: vec![MissingRuntime::Qemu(Architecture::X86_64)],
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("Qemu"));
    assert!(rendered.contains("install `qemu-system-x86_64`"));
}

#[test]
fn missing_qemu_hint_adapts_to_target_architecture() {
    let err = MissingRuntimeError {
        missing: vec![MissingRuntime::Qemu(Architecture::Aarch64)],
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("install `qemu-system-aarch64`"));
}

#[test]
fn forward_policy_detects_default_drop() {
    let ruleset = "\
table ip filter {
	chain FORWARD {
		type filter hook forward priority filter; policy drop;
	}
}
";
    assert_eq!(parse_forward_policy(ruleset), ForwardPolicy::Drop);
}

#[test]
fn forward_policy_accepts_when_no_forward_drop() {
    // An `accept` forward chain plus an unrelated `drop` (input hook) must
    // not be read as a blocking FORWARD policy.
    let ruleset = "\
table inet filter {
	chain forward {
		type filter hook forward priority filter; policy accept;
	}
	chain input {
		type filter hook input priority filter; policy drop;
	}
}
";
    assert_eq!(parse_forward_policy(ruleset), ForwardPolicy::Accept);
}

#[test]
fn forward_policy_accepts_empty_ruleset() {
    assert_eq!(parse_forward_policy(""), ForwardPolicy::Accept);
}
