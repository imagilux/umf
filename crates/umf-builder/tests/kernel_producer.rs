//! Regression test: a kernel producer build is a regular
//! container build with `org.imagilux.umf.type=kernel` (and the
//! vmlinuz + modules payload in the rootfs). There is no dedicated
//! `KERNEL` directive; this test locks in that the minimal producer
//! build still produces a correctly-labelled
//! artifact the consumer side (`umf_builder::introspect` +
//! `detect_kernel_layout`) recognises.
//!
//! No network, no qemu — entirely emit-then-introspect against a
//! synthetic layer that contains the expected `boot/vmlinuz-*` +
//! `lib/modules/*` structure.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;

use tempfile::tempdir;
use umf_builder::introspect::introspect;
use umf_core::l0::{L0Kind, L0Source, Payload};
use umf_core::label;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};
use umf_oci::registry::ImageLayout;

/// Helper: stage a tiny rootfs tree with `boot/vmlinuz-<release>` and
/// `lib/modules/<release>/` so the layer looks like what a real
/// kernel producer would emit.
fn populate_kernel_tree(root: &std::path::Path, release: &str) {
    let boot = root.join("boot");
    fs::create_dir_all(&boot).expect("mkdir boot");
    fs::write(
        boot.join(format!("vmlinuz-{release}")),
        b"fake-kernel-image\n",
    )
    .expect("write vmlinuz");

    let modules = root
        .join("lib")
        .join("modules")
        .join(release)
        .join("kernel");
    fs::create_dir_all(&modules).expect("mkdir modules");
    fs::write(modules.join("dummy.ko"), b"fake-module\n").expect("write module");
    fs::write(
        root.join("lib")
            .join("modules")
            .join(release)
            .join("modules.dep"),
        b"\n",
    )
    .expect("write modules.dep");
}

#[test]
fn kernel_producer_emits_introspectable_artifact() {
    let release = "6.6.79";

    // Stage the producer's rootfs payload.
    let tree = tempdir().expect("tree tempdir");
    populate_kernel_tree(tree.path(), release);
    let layer = LayerSource::from_directory(tree.path()).expect("build layer");

    // Emit an OCI image with the kernel labels — the kernel
    // producer-side contract: container-shape build, `umf_type =
    // Payload(Kernel)`, payload sitting at the canonical paths.
    let layout_dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let mut container = ContainerConfig::default();
    container.labels.insert(
        "org.imagilux.umf.kernel.release".to_string(),
        release.to_string(),
    );
    container.labels.insert(
        "org.imagilux.umf.kernel.config".to_string(),
        "default".to_string(),
    );
    let cfg = ImageConfig {
        container,
        umf_type: L0Kind::Payload(Payload::Kernel),
        ..ImageConfig::default()
    };

    let ref_name = "example.invalid/kernel-linux:6.6.79";
    emit_image(&layout, &[layer], &cfg, ref_name).expect("emit");

    // Consumer side: introspect must surface this as a kernel
    // payload, with the spec/type labels round-tripped.
    let profile = introspect(&layout, ref_name).expect("introspect");
    assert_eq!(
        profile.kind,
        L0Kind::Payload(Payload::Kernel),
        "umf_type=kernel should resolve to Payload(Kernel)",
    );
    assert_eq!(
        profile.source,
        L0Source::Label,
        "label provenance preferred over manifest-shape inference",
    );
    assert_eq!(
        profile.labels.get(label::TYPE).map(String::as_str),
        Some("kernel"),
        "type label must be present in the introspected profile",
    );
    assert_eq!(
        profile
            .labels
            .get("org.imagilux.umf.kernel.release")
            .map(String::as_str),
        Some(release),
        "release label must survive the round-trip",
    );
}

#[test]
fn kernel_build_env_emits_introspectable_artifact() {
    // The other half of the producer pattern: the build-env image
    // that a kernel producer FROMs. UMF carries this as a distinct
    // `kernel-build-env` shape so consumers know what they're
    // pulling.
    let layout_dir = tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::KernelBuildEnv,
        ..ImageConfig::default()
    };
    let ref_name = "example.invalid/kernel-build-env:1.0";
    emit_image(&layout, &[], &cfg, ref_name).expect("emit build-env");

    let profile = introspect(&layout, ref_name).expect("introspect");
    assert_eq!(profile.kind, L0Kind::KernelBuildEnv);
    assert_eq!(
        profile.labels.get(label::TYPE).map(String::as_str),
        Some("kernel-build-env"),
    );
}
