#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use umf_oci::image::{ImageConfig, emit_image};

/// A throwaway P-256 PKCS#8 key generated with
/// `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256`.
const ECDSA_P256_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgrQaVa9e07wtsQHYu
WHh1tH2CcikC6TCSK4rtnqrp+sKhRANCAATmgRIS2vIYT7NxxBBJKlFoDpyVhR06
mPJrqw7WaTBUTaZXQvn9Xn3Q3e7fElzsuFxU8AGqYatdpbMBBkVjA+v2
-----END PRIVATE KEY-----
";

/// A throwaway ed25519 PKCS#8 key generated with
/// `openssl genpkey -algorithm ed25519`.
const ED25519_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIFOQLn5qKrHSyoiUTXfEi4VKa+DEqiIt8qJ1Avgl0rIL
-----END PRIVATE KEY-----
";

#[test]
fn build_payload_is_cosign_simple_signing() {
    let bytes = build_payload("reg.example.com/app", "sha256:abc").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["critical"]["type"], "cosign container image signature");
    assert_eq!(
        v["critical"]["image"]["docker-manifest-digest"],
        "sha256:abc"
    );
    assert_eq!(
        v["critical"]["identity"]["docker-reference"],
        "reg.example.com/app"
    );
    assert!(v["optional"].is_null());
}

#[test]
fn ecdsa_signature_verifies_with_the_public_key() {
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
    use p256::pkcs8::DecodePrivateKey;

    let payload = b"payload-to-sign";
    let der = sign_ecdsa_p256(ECDSA_P256_PEM, payload).expect("sign");
    let signing = SigningKey::from_pkcs8_pem(ECDSA_P256_PEM).unwrap();
    let verifying: VerifyingKey = *signing.verifying_key();
    let signature = Signature::from_der(&der).unwrap();
    verifying
        .verify(payload, &signature)
        .expect("the DER signature verifies under the public key");
}

#[test]
fn ed25519_signature_verifies_with_the_public_key() {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    let payload = b"payload-to-sign";
    let raw = sign_ed25519(ED25519_PEM, payload).expect("sign");
    let signing = SigningKey::from_pkcs8_pem(ED25519_PEM).unwrap();
    let verifying = signing.verifying_key();
    let signature = Signature::from_slice(&raw).unwrap();
    verifying
        .verify(payload, &signature)
        .expect("the signature verifies under the public key");
}

#[test]
fn sign_payload_auto_detects_the_key_type() {
    let payload = b"x";
    let (kind, _) = sign_payload(ECDSA_P256_PEM, payload, None).unwrap();
    assert!(matches!(kind, SignKeyType::EcdsaP256));
    let (kind, _) = sign_payload(ED25519_PEM, payload, None).unwrap();
    assert!(matches!(kind, SignKeyType::Ed25519));
}

#[test]
fn run_sign_attaches_a_cosign_signature_referrer() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let subject = emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:1",
    )
    .unwrap();

    let keydir = TempDir::new().unwrap();
    let keypath = keydir.path().join("cosign.key");
    std::fs::write(&keypath, ECDSA_P256_PEM).unwrap();

    run_sign(SignArgs {
        reference: "example.invalid/app:1",
        key: &format!("id=signing-key,src={}", keypath.display()),
        key_type: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect("sign succeeds");

    let referrers = layout.list_referrers(&subject.digest, None).unwrap();
    assert_eq!(referrers.len(), 1, "one cosign signature referrer");
    assert_eq!(
        referrers[0].artifact_type.as_deref(),
        Some(COSIGN_SIG_ARTIFACT_TYPE),
    );
}

#[test]
fn run_sign_rejects_a_bad_key_spec() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    emit_image(
        &layout,
        &[],
        &ImageConfig::default(),
        "example.invalid/app:2",
    )
    .unwrap();

    let err = run_sign(SignArgs {
        reference: "example.invalid/app:2",
        key: "nonsense-without-id",
        key_type: None,
        push: false,
        layout_dir_override: Some(layout_dir.path()),
        insecure_registry: false,
        username: None,
        password_stdin: false,
    })
    .expect_err("a malformed --key spec must error");
    assert!(matches!(err, CliSignError::KeySpec(_)), "got {err:?}");
}
