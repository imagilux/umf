//! Unit tests for the `l0` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn from_label_recognises_all_documented_values() {
    assert_eq!(L0Kind::from_label("container"), L0Kind::Container);
    assert_eq!(L0Kind::from_label("bootable"), L0Kind::Bootable);
    assert_eq!(
        L0Kind::from_label("kernel-build-env"),
        L0Kind::KernelBuildEnv
    );
    assert_eq!(
        L0Kind::from_label("kernel"),
        L0Kind::Payload(Payload::Kernel)
    );
    assert_eq!(
        L0Kind::from_label("rootfs"),
        L0Kind::Payload(Payload::Rootfs)
    );
    assert_eq!(
        L0Kind::from_label("bootloader"),
        L0Kind::Payload(Payload::Bootloader),
    );
    assert_eq!(
        L0Kind::from_label("firmware"),
        L0Kind::Payload(Payload::Firmware),
    );
}

#[test]
fn from_label_preserves_unknown_values_verbatim() {
    assert_eq!(
        L0Kind::from_label("future-shape"),
        L0Kind::Unknown("future-shape".to_string()),
    );
    // Retired type values are no longer recognised — preserved verbatim.
    assert_eq!(L0Kind::from_label("vm"), L0Kind::Unknown("vm".to_string()));
    assert_eq!(
        L0Kind::from_label("bootc"),
        L0Kind::Unknown("bootc".to_string()),
    );
    assert_eq!(
        L0Kind::from_label("unikernel"),
        L0Kind::Unknown("unikernel".to_string()),
    );
    // Case-sensitive — capital `Container` is NOT mapped to the variant.
    assert_eq!(
        L0Kind::from_label("Container"),
        L0Kind::Unknown("Container".to_string()),
    );
}

#[test]
fn is_payload_only_true_for_payload_variants() {
    for kind in payload_variants() {
        assert!(kind.is_payload(), "{kind} should be payload");
    }
    for kind in non_payload_variants() {
        assert!(!kind.is_payload(), "{kind} should not be payload");
    }
}

#[test]
fn is_kernel_only_for_kernel_payload() {
    assert!(L0Kind::Payload(Payload::Kernel).is_kernel());
    for kind in [
        L0Kind::Scratch,
        L0Kind::Container,
        L0Kind::Bootable,
        L0Kind::KernelBuildEnv,
        L0Kind::Payload(Payload::Rootfs),
        L0Kind::Payload(Payload::Bootloader),
        L0Kind::Payload(Payload::Firmware),
        L0Kind::Unknown("xyz".into()),
    ] {
        assert!(!kind.is_kernel(), "{kind} should not be kernel");
    }
}

#[test]
fn is_valid_from_for_bootable_build_accepts_kernel_and_bootable() {
    assert!(L0Kind::Payload(Payload::Kernel).is_valid_from(true));
    assert!(L0Kind::Bootable.is_valid_from(true));
    for kind in [
        L0Kind::Scratch,
        L0Kind::Container,
        L0Kind::KernelBuildEnv,
        L0Kind::Payload(Payload::Rootfs),
        L0Kind::Payload(Payload::Bootloader),
        L0Kind::Payload(Payload::Firmware),
        L0Kind::Unknown("xyz".into()),
    ] {
        assert!(
            !kind.is_valid_from(true),
            "{kind} should not be a valid FROM for a bootable build",
        );
    }
}

#[test]
fn is_valid_from_for_container_build_accepts_container_shaped() {
    for kind in [
        L0Kind::Scratch,
        L0Kind::Container,
        L0Kind::KernelBuildEnv,
        L0Kind::Bootable,
    ] {
        assert!(
            kind.is_valid_from(false),
            "{kind} should be a valid FROM for a container build",
        );
    }
    for kind in [
        L0Kind::Payload(Payload::Kernel),
        L0Kind::Payload(Payload::Rootfs),
        L0Kind::Payload(Payload::Bootloader),
        L0Kind::Payload(Payload::Firmware),
        L0Kind::Unknown("xyz".into()),
    ] {
        assert!(
            !kind.is_valid_from(false),
            "{kind} should be rejected as FROM for a container build",
        );
    }
}

#[test]
fn display_round_trips_through_from_label() {
    for kind in [
        L0Kind::Container,
        L0Kind::Bootable,
        L0Kind::KernelBuildEnv,
        L0Kind::Payload(Payload::Kernel),
        L0Kind::Payload(Payload::Rootfs),
        L0Kind::Payload(Payload::Bootloader),
        L0Kind::Payload(Payload::Firmware),
    ] {
        let display = kind.to_string();
        assert_eq!(L0Kind::from_label(&display), kind, "round trip {display}");
    }
}

fn payload_variants() -> Vec<L0Kind> {
    vec![
        L0Kind::Payload(Payload::Kernel),
        L0Kind::Payload(Payload::Rootfs),
        L0Kind::Payload(Payload::Bootloader),
        L0Kind::Payload(Payload::Firmware),
    ]
}

fn non_payload_variants() -> Vec<L0Kind> {
    vec![
        L0Kind::Scratch,
        L0Kind::Container,
        L0Kind::Bootable,
        L0Kind::KernelBuildEnv,
        L0Kind::Unknown("xyz".into()),
    ]
}
