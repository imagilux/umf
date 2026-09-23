//! Unit tests for the `noop` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn parse_user_spec_accepts_uid_and_uid_gid() {
    let u = parse_user_spec("1000").expect("uid only");
    assert_eq!(u.uid(), 1000);
    assert_eq!(u.gid(), 0);

    let u = parse_user_spec("1000:1001").expect("uid:gid");
    assert_eq!(u.uid(), 1000);
    assert_eq!(u.gid(), 1001);
}

#[test]
fn parse_user_spec_rejects_garbage() {
    assert!(matches!(
        parse_user_spec("bob").unwrap_err(),
        EngineError::Runtime { .. }
    ));
}

/// The spec round-trip must preserve LSM confinement.
///
/// `apply_run_spec_to_bundle` rebuilds `process` from a fresh
/// `ProcessBuilder::default()` and copies fields back one at a time. Anything
/// it forgets is silently dropped from the spec the runtime actually receives
/// — so an operator who set `UMF_APPARMOR_PROFILE` / `UMF_SELINUX_LABEL` gets
/// an unconfined RUN step while believing confinement is on.
///
/// It is invisible on a host with no LSM loaded, which is most CI, so nothing
/// would surface it except this assertion.
#[test]
fn the_spec_rebuild_preserves_lsm_confinement() {
    let mut bundle = crate::bundle::Bundle::from_scratch(&crate::bundle::BundleOptions::default())
        .expect("scratch bundle");

    // Put confinement on the spec exactly where `build_runtime_spec` puts it.
    {
        let spec = bundle.spec_mut();
        let existing = spec.process().clone().expect("scratch spec has a process");
        let confined = oci_spec::runtime::ProcessBuilder::default()
            .args(existing.args().clone().unwrap_or_default())
            .cwd(existing.cwd().clone())
            .apparmor_profile("umf-confined".to_string())
            .selinux_label("system_u:system_r:umf_t:s0".to_string())
            .build()
            .expect("confined process");
        spec.set_process(Some(confined));
    }

    let run_spec = RunSpec::new("lsm-round-trip", ["/bin/true"]);
    apply_run_spec_to_bundle(&mut bundle, &run_spec).expect("apply run spec");

    let process = bundle
        .spec_mut()
        .process()
        .clone()
        .expect("process after rebuild");
    assert_eq!(
        process.apparmor_profile().as_deref(),
        Some("umf-confined"),
        "the AppArmor profile was dropped by the spec rebuild — the RUN step \
         would execute unconfined",
    );
    assert_eq!(
        process.selinux_label().as_deref(),
        Some("system_u:system_r:umf_t:s0"),
        "the SELinux label was dropped by the spec rebuild",
    );
}
