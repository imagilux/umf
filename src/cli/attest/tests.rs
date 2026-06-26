#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use umf_oci::image::{ImageConfig, emit_image};

/// Same throwaway P-256 key as the `sign` tests.
const ECDSA_P256_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgrQaVa9e07wtsQHYu
WHh1tH2CcikC6TCSK4rtnqrp+sKhRANCAATmgRIS2vIYT7NxxBBJKlFoDpyVhR06
mPJrqw7WaTBUTaZXQvn9Xn3Q3e7fElzsuFxU8AGqYatdpbMBBkVjA+v2
-----END PRIVATE KEY-----
";

#[test]
fn dsse_pae_matches_the_spec_encoding() {
    // DSSEv1 SP len(type) SP type SP len(body) SP body
    let pae = dsse_pae("application/x", b"hi");
    assert_eq!(pae, b"DSSEv1 13 application/x 2 hi");
}

#[test]
fn predicate_type_shorthands_resolve() {
    assert_eq!(
        predicate_type_uri("slsaprovenance"),
        "https://slsa.dev/provenance/v1"
    );
    assert_eq!(predicate_type_uri("spdx"), "https://spdx.dev/Document");
    // A full URI passes through untouched.
    assert_eq!(
        predicate_type_uri("https://example.com/x"),
        "https://example.com/x"
    );
}

#[test]
fn dsse_signature_verifies_over_the_pae() {
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
    use p256::pkcs8::DecodePrivateKey;

    let statement = br#"{"_type":"https://in-toto.io/Statement/v1"}"#;
    let pae = dsse_pae(IN_TOTO_PAYLOAD_TYPE, statement);
    let (_, der) = sign_payload(ECDSA_P256_PEM, &pae, Some(SignKeyType::EcdsaP256)).unwrap();

    let verifying: VerifyingKey = *SigningKey::from_pkcs8_pem(ECDSA_P256_PEM)
        .unwrap()
        .verifying_key();
    let signature = Signature::from_der(&der).unwrap();
    verifying
        .verify(&pae, &signature)
        .expect("the DSSE PAE signature verifies under the public key");
}

#[test]
fn run_attest_attaches_a_dsse_referrer_with_the_predicate_type() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let subject = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:1",
    )
    .unwrap();

    let dir = TempDir::new().unwrap();
    let keypath = dir.path().join("cosign.key");
    std::fs::write(&keypath, ECDSA_P256_PEM).unwrap();
    let predpath = dir.path().join("predicate.json");
    std::fs::write(
        &predpath,
        br#"{"buildType":"https://umf.imagilux.org/bt/v1"}"#,
    )
    .unwrap();

    run_attest(AttestArgs {
        reference: "example.invalid/app:1",
        predicate: &predpath,
        predicate_type: "slsaprovenance",
        key: &format!("id=signing-key,src={}", keypath.display()),
        key_type: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect("attest succeeds");

    let referrers = layout.list_referrers(&subject.digest, None).unwrap();
    assert_eq!(referrers.len(), 1, "one attestation referrer");
    assert_eq!(
        referrers[0].artifact_type.as_deref(),
        Some(COSIGN_SIG_ARTIFACT_TYPE),
    );
    assert_eq!(
        referrers[0]
            .annotations
            .as_ref()
            .and_then(|a| a.get("predicateType"))
            .map(String::as_str),
        Some("https://slsa.dev/provenance/v1"),
    );
}

#[test]
fn run_attest_rejects_a_non_json_predicate() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:2",
    )
    .unwrap();

    let dir = TempDir::new().unwrap();
    let keypath = dir.path().join("cosign.key");
    std::fs::write(&keypath, ECDSA_P256_PEM).unwrap();
    let predpath = dir.path().join("predicate.json");
    std::fs::write(&predpath, b"this is not json").unwrap();

    let err = run_attest(AttestArgs {
        reference: "example.invalid/app:2",
        predicate: &predpath,
        predicate_type: "slsaprovenance",
        key: &format!("id=signing-key,src={}", keypath.display()),
        key_type: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect_err("a non-JSON predicate must error");
    assert!(
        matches!(err, CliAttestError::PredicateJson(_)),
        "got {err:?}"
    );
}
