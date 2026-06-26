//! End-to-end smoke test for `umf_engine::run_image`.
//!
//! Pulls `alpine:3.21`, runs it via `run_image` with a `--cmd` override
//! that prints a sentinel and exits non-zero, then verifies that the
//! exit code makes it back to the caller.
//!
//! Gated behind `UMF_ENGINE_SMOKE=1` because it needs network + root
//! (kernel overlayfs requires `CAP_SYS_ADMIN`). Environmental pull
//! failures (rate-limit, transient network) skip rather than fail —
//! the test is advisory.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use tempfile::TempDir;
use umf_engine::{RunOptions, run_image};
use umf_oci::registry::{ImageLayout, Reference, RegistryAuth, RegistryClient};

const SMOKE_ENV: &str = "UMF_ENGINE_SMOKE";

fn smoke_enabled() -> bool {
    std::env::var(SMOKE_ENV).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

fn is_pull_environmental(err: &dyn std::fmt::Display) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("toomanyrequests")
        || s.contains("rate limit")
        || s.contains("name or service not known")
        || s.contains("connection refused")
        || s.contains("network is unreachable")
}

/// Base image ref, overridable via `UMF_SMOKE_BASE_IMAGE` so CI can pull from a
/// registry that doesn't anonymously rate-limit (GHCR / our own) instead of
/// Docker Hub. Defaults to the canonical alpine ref for local runs.
fn base_ref() -> String {
    std::env::var("UMF_SMOKE_BASE_IMAGE")
        .unwrap_or_else(|_| "docker.io/library/alpine:3.21".to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_image_propagates_exit_code() {
    if !smoke_enabled() {
        eprintln!(
            "skipping {SMOKE_ENV}-gated smoke test (set {SMOKE_ENV}=1 to run; needs network + CAP_SYS_ADMIN)"
        );
        return;
    }

    // Pull a tiny base image into a fresh layout.
    let layout_dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let ref_name = base_ref();
    let ref_name = ref_name.as_str();
    let reference: Reference = ref_name.parse().expect("parse alpine reference");
    let client = RegistryClient::new();
    if let Err(err) = client
        .pull(&reference, &RegistryAuth::Anonymous, &layout)
        .await
    {
        if is_pull_environmental(&err) {
            eprintln!("skipping: pull failed environmentally ({err})");
            return;
        }
        panic!("pull failed: {err}");
    }

    // Run with a CMD override that exits 42 — the exact value should
    // round-trip through libcontainer's waitpid back to our caller.
    // alpine's image-config has an empty Entrypoint, so the bundle
    // uses cmd as-is; supplying only cmd_override is the right shape.
    let options = RunOptions {
        cmd_override: Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 42".to_string(),
        ]),
        ..RunOptions::default()
    };

    let result = run_image(&layout, ref_name, &options).expect("run_image");
    assert_eq!(
        result.exit_code,
        Some(42),
        "exit code must round-trip through libcontainer",
    );
    assert!(
        result.bundle_path.is_none(),
        "default keep_bundle=false should drop the bundle",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_image_keep_bundle_preserves_dir() {
    if !smoke_enabled() {
        eprintln!(
            "skipping {SMOKE_ENV}-gated smoke test (set {SMOKE_ENV}=1 to run; needs network + CAP_SYS_ADMIN)"
        );
        return;
    }

    let layout_dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let ref_name = base_ref();
    let ref_name = ref_name.as_str();
    let reference: Reference = ref_name.parse().expect("parse alpine reference");
    let client = RegistryClient::new();
    if let Err(err) = client
        .pull(&reference, &RegistryAuth::Anonymous, &layout)
        .await
    {
        if is_pull_environmental(&err) {
            eprintln!("skipping: pull failed environmentally ({err})");
            return;
        }
        panic!("pull failed: {err}");
    }

    let options = RunOptions {
        cmd_override: Some(vec!["/bin/true".to_string()]),
        keep_bundle: true,
        ..RunOptions::default()
    };

    let result = run_image(&layout, ref_name, &options).expect("run_image");
    let bundle_path = result.bundle_path.expect("keep_bundle should preserve");
    assert!(
        bundle_path.join("rootfs").exists(),
        "preserved bundle should contain a rootfs/ directory",
    );
    assert!(
        bundle_path.join("config.json").exists(),
        "preserved bundle should contain a config.json file",
    );

    // We own the directory now — clean it up so the test doesn't litter
    // /tmp on repeated runs.
    let _ = std::fs::remove_dir_all(&bundle_path);
}
