//! Unit tests for the `label` module.

use super::*;

/// Every UMF label key must live under the `org.imagilux.umf` namespace.
#[test]
fn all_label_keys_are_namespaced() {
    let prefix = format!("{NAMESPACE}.");
    for key in [
        TYPE,
        SPEC_VERSION,
        ENTRYPOINT,
        KERNEL_RELEASE,
        KERNEL_VMLINUZ,
        KERNEL_CMDLINE,
        INITRAMFS,
        ROOTFS_FS,
        FLAVOR,
    ] {
        assert!(
            key.starts_with(&prefix),
            "label key `{key}` is outside the `{NAMESPACE}` namespace",
        );
    }
}
