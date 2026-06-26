//! End-to-end smoke test for the libcontainer-backed engine.
//!
//! Pulls `alpine:3.21`, prepares a bundle from it, wraps the rootfs in
//! an overlay, runs `/bin/sh -c 'echo hi > /out'`, and confirms `/out`
//! shows up in the captured upper-dir.
//!
//! Gated behind `UMF_ENGINE_SMOKE=1` and skipped by default — running
//! it requires:
//!
//! 1. Network access to pull `alpine:3.21` from the registry the
//!    reference points to (the umf-oci registry client does the pull).
//! 2. Kernel overlayfs + `CAP_SYS_ADMIN` for the overlay mount. In this
//!    revision that means root or a setuid umf-engine binary; rootless
//!    overlay needs fuse-overlayfs.
//!
//! When the env var isn't set, the test prints a one-line "skipping"
//! note and returns. When it is set but the requirements aren't met,
//! the test fails with a clear diagnostic.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use tempfile::TempDir;
use umf_engine::backends::LibcontainerRuntime;
use umf_engine::bundle::{Bundle, BundleOptions, LayerStrategy};
use umf_engine::overlay::Overlay;
use umf_engine::runtime::{ContainerRuntime, RunSpec};
use umf_oci::registry::{ImageLayout, Reference, RegistryAuth, RegistryClient};

const SMOKE_ENV: &str = "UMF_ENGINE_SMOKE";

fn smoke_enabled() -> bool {
    std::env::var(SMOKE_ENV).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

#[tokio::test]
async fn alpine_echo_lands_in_upper_dir() {
    if !smoke_enabled() {
        eprintln!(
            "skipping {SMOKE_ENV}-gated smoke test (set {SMOKE_ENV}=1 to run; needs network + CAP_SYS_ADMIN)"
        );
        return;
    }

    // 1. Layout + pull alpine:3.21.
    let layout_dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    // Overridable (UMF_SMOKE_BASE_IMAGE) so CI can pull from a registry that
    // doesn't anonymously rate-limit (GHCR / our own) instead of Docker Hub.
    let ref_name_owned = std::env::var("UMF_SMOKE_BASE_IMAGE")
        .unwrap_or_else(|_| "docker.io/library/alpine:3.21".to_string());
    let ref_name = ref_name_owned.as_str();
    let reference: Reference = ref_name.parse().expect("parse alpine reference");
    let client = RegistryClient::new();
    if let Err(err) = client
        .pull(&reference, &RegistryAuth::Anonymous, &layout)
        .await
    {
        let s = err.to_string().to_lowercase();
        if s.contains("toomanyrequests")
            || s.contains("rate limit")
            || s.contains("name or service not known")
            || s.contains("connection refused")
        {
            eprintln!("skipping: environmental pull failure: {err}");
            return;
        }
        panic!("pull alpine:3.21 failed: {err}");
    }

    // 2. Bundle prep with rootful options (overlay needs CAP_SYS_ADMIN
    //    and the libcontainer backend needs to map uid 0 → uid 0 in
    //    this revision).
    let opts = BundleOptions {
        hostname: "umf-smoke".to_string(),
        rootless: false,
        host_uid: 0,
        host_gid: 0,
        // This test overlays directly on bundle.rootfs(), so it needs the
        // merged tree (the erofs path leaves rootfs empty).
        layer_strategy: LayerStrategy::Merge,
        ..BundleOptions::default()
    };
    let mut bundle = Bundle::from_image(&layout, ref_name, &opts).expect("bundle prep");

    // 3. Mount overlay over the bundle's rootfs; the upper-dir captures
    //    the diff. The engine itself doesn't care about overlays — this
    //    is the caller's responsibility under the contract.
    let overlay = Overlay::mount(&[bundle.rootfs()]).expect("overlay mount");
    bundle
        .set_root_path(overlay.merged())
        .expect("redirect bundle rootfs");

    // 4. Run `/bin/sh -c 'echo hi > /out'` via libcontainer.
    let state_root = TempDir::new().expect("state-root tempdir");
    let runtime = LibcontainerRuntime::new(state_root.path()).expect("runtime ctor");
    let spec = RunSpec {
        id: format!("umf-smoke-{}", std::process::id()),
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hi > /out".to_string(),
        ],
        env: Vec::new(),
        working_dir: Some("/".to_string()),
        user: None,
        bind_mounts: Vec::new(),
    };

    let outcome = runtime.run(&mut bundle, &spec).expect("engine run");
    assert!(
        outcome.is_success(),
        "container should exit 0; got exit_code={:?}",
        outcome.exit_code,
    );

    // 5. Persist the upper-dir before the overlay is dropped, then
    //    confirm /out is captured.
    let upper = overlay.persist_upper().expect("persist upper");
    let upper_dir: &std::path::Path = upper.path();
    let written = std::fs::read_to_string(upper_dir.join("out")).unwrap_or_else(|e| {
        panic!(
            "expected /out in upper-dir {upper_dir:?}; got error: {e}. \
            Upper-dir contents:\n{:#?}",
            std::fs::read_dir(upper_dir)
                .ok()
                .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
        );
    });
    assert_eq!(written.trim(), "hi");
}
