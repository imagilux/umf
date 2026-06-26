//! Unit tests for the `vm_runner` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn short_command_truncates_long_lines() {
    let long = "a".repeat(200);
    let s = short_command(&long);
    assert!(s.ends_with("..."));
    assert!(s.chars().count() < 80);
}

#[test]
fn short_command_is_char_safe_at_the_boundary() {
    // 63 ASCII + a multi-byte char straddling byte 64: the old byte-slice
    // `&oneline[..64]` panicked here.
    let cmd = format!("{}€-tail-padding-to-exceed-the-limit", "b".repeat(63));
    let s = short_command(&cmd);
    assert!(s.ends_with("..."));
    assert!(s.chars().count() <= 67);
}

#[tokio::test]
async fn qemu_missing_path_fails_fast() {
    let staging = BuildStaging::new().expect("staging");
    let kernel = KernelLayout {
        release: "6.6.79".into(),
        vmlinuz: staging.path().join("boot/vmlinuz-6.6.79"),
        modules: staging.path().join("lib/modules/6.6.79"),
    };
    let config = RunStepConfig::new(PathBuf::from("/does/not/exist/qemu"), false, "true".into());
    let err = run_step_vm(&staging, &kernel, &config).await.unwrap_err();
    assert!(matches!(err, RunStepError::QemuMissing));
}

#[test]
fn parse_meminfo_total_mib_extracts_memtotal() {
    let sample = "MemTotal:       16312456 kB\nMemFree:          123456 kB\n";
    assert_eq!(parse_meminfo_total_mib(sample), Some(16_312_456 / 1024));
    // No MemTotal line, and a malformed value, both yield None (caller floors).
    assert_eq!(parse_meminfo_total_mib("MemFree: 1 kB\n"), None);
    assert_eq!(parse_meminfo_total_mib("MemTotal: not-a-number kB\n"), None);
}

#[test]
fn run_resource_defaults_respect_their_floors() {
    // Whatever the host size, and even with `UMF_RUN_*` overrides set, the
    // derived defaults never fall below the documented floors (the audit-E
    // failure was a *fixed* 512 MiB / 2 vCPU regardless of host).
    assert!(default_run_cpus() >= 1);
    assert!(default_run_memory_mib() >= MIN_RUN_MEMORY_MIB);
}

#[test]
fn run_timeout_is_longer_under_tcg_than_kvm() {
    // Only meaningful without an explicit override (which pins both equal).
    if std::env::var_os("UMF_RUN_TIMEOUT_SECS").is_none() {
        assert_eq!(
            default_run_timeout(true),
            Duration::from_secs(RUN_TIMEOUT_BASE_SECS)
        );
        assert!(default_run_timeout(false) > default_run_timeout(true));
    }
}
