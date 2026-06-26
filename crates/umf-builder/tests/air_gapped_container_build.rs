//! Sovereignty verification: a container build whose `FROM` is
//! already cached in the local layout must complete with **no
//! registry access**. This locks in the project's "sovereignty-first"
//! pillar — an air-gapped node, given a pre-populated layout, can
//! build new images from the cached components alone.
//!
//! The test simulates an operator's pre-warmed layout:
//!
//! 1. Stage a synthetic FROM image **directly in the operator's
//!    layout** via `umf-oci::image::emit_image`. The real-world
//!    equivalent is `umf pull <ref>` (or `umf load <archive.tar>`)
//!    against an internet-reachable registry; the result is the
//!    same — a fully resolvable manifest + blobs in the layout
//!    keyed by the operator's chosen ref name.
//! 2. Run a small container build whose `FROM` references that
//!    cached image. The build adds a LABEL — no RUN steps so no
//!    libcontainer spawn is needed (the test stays runnable on
//!    any contributor laptop, no `CAP_SYS_ADMIN`, no network).
//! 3. Assert the build produces a new image in the layout. Since
//!    the FROM is already cached, the build's `pull_into_layout`
//!    short-circuits — no registry endpoint is contacted at all,
//!    so the build succeeds without any network reachability.
//!
//! There is no point setting `unshare -n` or similar around the
//! test: the assertion isn't "the build copes when network is
//! disabled," it's "the build doesn't need network in the first
//! place when its inputs are pre-cached." Proving the latter
//! proves the former.
//!
//! VM-target sovereignty verification is **deliberately out of scope**
//! here — it requires a kernel artifact, rootfs artifact, and
//! bootloader artifact all pre-cached, and the bootable-target work
//! needs to land first. The container-target test below
//! is the minimum that proves the architectural pillar; the VM-target
//! equivalent is a follow-up once the bootable targets are in.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use tempfile::tempdir;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};
use umf_oci::registry::ImageLayout;

fn populate_minimal_rootfs(root: &Path) {
    // The smallest tree that still parses as a real OCI layer when
    // unpacked. The bundle path only needs a rootfs directory to
    // exist + a single file in it (the unpacker doesn't otherwise
    // care about contents).
    std::fs::create_dir_all(root).expect("mkdir root");
    std::fs::write(root.join(".keep"), b"\n").expect("write keep");
}

#[tokio::test]
async fn container_build_succeeds_with_no_registry_access() {
    use umf_builder::engine_build::{EngineBuildOptions, build};
    use umf_core::ast::Ast;

    // ── 1. Stage a synthetic FROM image directly in the operator's
    //       layout (simulating an earlier `umf pull` or `umf load`).
    let operator_dir = tempdir().expect("operator tempdir");
    let operator_layout = ImageLayout::init(operator_dir.path()).expect("init operator");

    let producer_tree = tempdir().expect("producer rootfs");
    populate_minimal_rootfs(producer_tree.path());
    let base_layer =
        LayerSource::from_directory(producer_tree.path()).expect("build producer layer");

    // Use a hostname-shaped ref the parser accepts. Note: nothing
    // ever resolves this hostname during the test — the build will
    // hit the cache and short-circuit before any DNS or HTTP would
    // happen.
    let base_ref = "registry.example.com/airgap/base:latest";

    let base_cfg = ImageConfig {
        container: ContainerConfig {
            entrypoint: Some(vec!["/bin/sh".to_string()]),
            env: vec!["PATH=/usr/local/bin:/usr/bin".to_string()],
            ..ContainerConfig::default()
        },
        umf_type: umf_core::l0::L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&operator_layout, &[base_layer], &base_cfg, base_ref)
        .expect("emit base into operator layout");

    // Sanity: the operator's layout knows about the base.
    assert!(
        operator_layout
            .lookup_ref(base_ref)
            .expect("lookup")
            .is_some(),
        "operator layout should have the cached base before the build",
    );

    // ── 2. Build a new image FROM the cached base. ──────────────────────
    // The recipe is metadata-only (just a LABEL) so we don't need to
    // run any container — no libcontainer / CAP_SYS_ADMIN required.
    let ast: Ast = umf_parser::parse(&format!(
        "FROM {base_ref}\n\
         LABEL air-gapped.test=success\n",
    ))
    .expect("parse");

    let context_dir = tempdir().expect("ctx tempdir");
    let final_ref = "airgap-test/derivative:1";
    let entry = build(
        &operator_layout,
        context_dir.path(),
        &ast,
        final_ref,
        &EngineBuildOptions::default(),
    )
    .await
    .expect("air-gapped build should succeed against cached layout");

    // ── 3. Assert the derivative landed. ────────────────────────────────
    let derivative = operator_layout
        .lookup_ref(final_ref)
        .expect("lookup derivative")
        .expect("derivative ref must be registered");
    assert_eq!(derivative.digest, entry.digest);

    // And — for paranoid clarity — confirm the cached base is also
    // still resolvable. (The build shouldn't have evicted it.)
    assert!(
        operator_layout
            .lookup_ref(base_ref)
            .expect("lookup base after build")
            .is_some(),
        "base still cached after build",
    );
}
