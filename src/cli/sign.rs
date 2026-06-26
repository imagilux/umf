//! `umf sign` — attach a cosign-compatible static-key signature to an image
//! as an OCI 1.1 referrer artifact.
//!
//! Signs the cosign "simple signing" payload (the image's manifest digest +
//! reference) with a PKCS#8 PEM private key supplied through the same
//! `--secret`-style spec as `umf build` (`id=…,src=…` / `id=…,env=…`), then
//! attaches it as a referrer: artifactType
//! `application/vnd.dev.cosign.artifact.sig.v1+json`, a `…simplesigning.v1+json`
//! blob carrying the payload, and the base64 signature in the
//! `dev.cosignproject.cosign/signature` annotation. That is exactly cosign's
//! OCI-1.1 attachment, so `cosign verify --key <pub>` (and `oras discover`)
//! read it back.
//!
//! Static-key only — ECDSA P-256 (cosign's default) or ed25519, both
//! pure-Rust (no OpenSSL) and deterministic. Sigstore keyless (OIDC + Fulcio +
//! Rekor) is intentionally out of scope for an air-gapped tool.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use oci_client::Reference;
use serde::Serialize;
use thiserror::Error;
use tracing::info;
use umf_oci::image::{ArtifactBlob, emit_artifact_manifest, subject_from_entry};
use umf_oci::registry::{ImageLayout, RegistryError};

use crate::cli::SignKeyType;
use crate::cli::build::{parse_secret_spec, read_secret_material};
use crate::cli::util::{self, CredentialError};

/// cosign signature referrer artifactType.
const COSIGN_SIG_ARTIFACT_TYPE: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";
/// Media type of the simple-signing payload blob.
const SIMPLE_SIGNING_MEDIA_TYPE: &str = "application/vnd.dev.cosign.simplesigning.v1+json";
/// Annotation carrying the base64 signature (cosign's key).
const COSIGN_SIGNATURE_ANNOTATION: &str = "dev.cosignproject.cosign/signature";

#[derive(Debug, Error)]
pub(crate) enum CliSignError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
    #[error("invalid OCI reference {reference:?}: {err}")]
    BadReference {
        reference: String,
        err: oci_client::ParseError,
    },
    #[error("image not found in layout: {0} (build or pull it first, e.g. `umf pull {0}`)")]
    ImageNotFound(String),
    #[error("--key {0}")]
    KeySpec(String),
    #[error("read signing key: {0}")]
    KeyRead(std::io::Error),
    #[error("signing key is not valid UTF-8 PEM")]
    KeyNotPem,
    #[error("could not load the signing key as a {key_type} PKCS#8 PEM private key: {detail}")]
    KeyLoad {
        key_type: &'static str,
        detail: String,
    },
    #[error("payload JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl From<CredentialError> for CliSignError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf sign` flags.
pub(crate) struct SignArgs<'a> {
    /// Image to sign (must be present in the local layout).
    pub(crate) reference: &'a str,
    /// Signing-key spec (`id=…,src=…` / `id=…,env=…`), same grammar as
    /// `umf build --secret`.
    pub(crate) key: &'a str,
    /// Key algorithm; auto-detected from the key when `None`.
    pub(crate) key_type: Option<SignKeyType>,
    pub(crate) push: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
}

/// Sign `reference` and attach the cosign signature as a referrer.
pub(crate) fn run_sign(args: SignArgs<'_>) -> Result<(), CliSignError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliSignError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;

    let subject_entry = layout
        .lookup_ref(args.reference)?
        .ok_or_else(|| CliSignError::ImageNotFound(args.reference.to_string()))?;
    let reference: Reference = args
        .reference
        .parse()
        .map_err(|err: oci_client::ParseError| CliSignError::BadReference {
            reference: args.reference.to_string(),
            err,
        })?;

    // Load the signing key through the build-secret channel.
    let spec = parse_secret_spec(args.key).map_err(CliSignError::KeySpec)?;
    let key_bytes = read_secret_material(&spec).map_err(CliSignError::KeyRead)?;
    let key_pem = std::str::from_utf8(&key_bytes).map_err(|_| CliSignError::KeyNotPem)?;

    // Build + sign the cosign simple-signing payload over the manifest digest.
    let docker_reference = format!("{}/{}", reference.registry(), reference.repository());
    let payload = build_payload(&docker_reference, &subject_entry.digest)?;
    let (algo, signature) = sign_payload(key_pem, &payload, args.key_type)?;
    let signature_b64 = base64::engine::general_purpose::STANDARD.encode(&signature);

    // Attach as a cosign-shaped referrer.
    let subject = subject_from_entry(&subject_entry);
    let mut annotations = BTreeMap::new();
    annotations.insert(COSIGN_SIGNATURE_ANNOTATION.to_string(), signature_b64);
    let blob = ArtifactBlob {
        media_type: SIMPLE_SIGNING_MEDIA_TYPE.to_string(),
        data: bytes::Bytes::from(payload),
        annotations: Some(annotations),
    };
    let entry = emit_artifact_manifest(
        &layout,
        COSIGN_SIG_ARTIFACT_TYPE,
        Some(&subject),
        std::slice::from_ref(&blob),
        None,
        None,
    )?;
    info!(subject = %subject.digest, signature = %entry.digest, algo = algo_label(algo), "signed image");
    println!(
        "Signed {subject} with {algo} -> signature {sig}",
        subject = subject.digest,
        algo = algo_label(algo),
        sig = entry.digest,
    );

    if args.push {
        util::push_referrer_for::<CliSignError>(
            &layout,
            &reference,
            args.username,
            args.password_stdin,
            args.insecure_registry,
            &entry,
        )?;
    }
    Ok(())
}

// ── cosign simple-signing payload ─────────────────────────────────────────

#[derive(Serialize)]
struct SimpleSigning {
    critical: Critical,
    optional: Option<serde_json::Value>,
}
#[derive(Serialize)]
struct Critical {
    identity: Identity,
    image: Image,
    #[serde(rename = "type")]
    kind: &'static str,
}
#[derive(Serialize)]
struct Identity {
    #[serde(rename = "docker-reference")]
    docker_reference: String,
}
#[derive(Serialize)]
struct Image {
    #[serde(rename = "docker-manifest-digest")]
    docker_manifest_digest: String,
}

/// Serialize the cosign simple-signing payload. The exact bytes are both
/// signed and stored, so a verifier checks the signature against this blob.
fn build_payload(docker_reference: &str, manifest_digest: &str) -> Result<Vec<u8>, CliSignError> {
    let payload = SimpleSigning {
        critical: Critical {
            identity: Identity {
                docker_reference: docker_reference.to_string(),
            },
            image: Image {
                docker_manifest_digest: manifest_digest.to_string(),
            },
            kind: "cosign container image signature",
        },
        optional: None,
    };
    Ok(serde_json::to_vec(&payload)?)
}

// ── signing ───────────────────────────────────────────────────────────────

fn algo_label(key_type: SignKeyType) -> &'static str {
    match key_type {
        SignKeyType::EcdsaP256 => "ecdsa-p256",
        SignKeyType::Ed25519 => "ed25519",
    }
}

/// Sign `payload`, honoring an explicit `key_type` or auto-detecting (ECDSA
/// P-256, cosign's default, then ed25519).
pub(crate) fn sign_payload(
    key_pem: &str,
    payload: &[u8],
    key_type: Option<SignKeyType>,
) -> Result<(SignKeyType, Vec<u8>), CliSignError> {
    match key_type {
        Some(SignKeyType::EcdsaP256) => {
            Ok((SignKeyType::EcdsaP256, sign_ecdsa_p256(key_pem, payload)?))
        }
        Some(SignKeyType::Ed25519) => Ok((SignKeyType::Ed25519, sign_ed25519(key_pem, payload)?)),
        None => match sign_ecdsa_p256(key_pem, payload) {
            Ok(sig) => Ok((SignKeyType::EcdsaP256, sig)),
            Err(_) => Ok((SignKeyType::Ed25519, sign_ed25519(key_pem, payload)?)),
        },
    }
}

/// ECDSA P-256 over SHA-256, deterministic (RFC 6979), ASN.1 DER — cosign's
/// signature shape for an EC key.
fn sign_ecdsa_p256(key_pem: &str, payload: &[u8]) -> Result<Vec<u8>, CliSignError> {
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let key = SigningKey::from_pkcs8_pem(key_pem).map_err(|e| CliSignError::KeyLoad {
        key_type: "ECDSA-P256",
        detail: e.to_string(),
    })?;
    let signature: Signature = key.sign(payload);
    Ok(signature.to_der().as_bytes().to_vec())
}

/// ed25519 over the raw payload.
fn sign_ed25519(key_pem: &str, payload: &[u8]) -> Result<Vec<u8>, CliSignError> {
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::DecodePrivateKey;

    let key = SigningKey::from_pkcs8_pem(key_pem).map_err(|e| CliSignError::KeyLoad {
        key_type: "ed25519",
        detail: e.to_string(),
    })?;
    Ok(key.sign(payload).to_bytes().to_vec())
}

#[cfg(test)]
mod tests;
