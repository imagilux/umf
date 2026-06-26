//! `umf attest` — attach a signed in-toto/SLSA attestation to an image as an
//! OCI 1.1 referrer artifact.
//!
//! Wraps a user-supplied predicate in an in-toto Statement (whose `subject`
//! is the image's manifest digest), signs the DSSE Pre-Authentication
//! Encoding of that statement with the same static-key channel as `umf sign`,
//! and attaches the DSSE envelope as a referrer (blob media type
//! `application/vnd.dsse.envelope.v1+json`, a `predicateType` annotation).
//! This is cosign's attestation shape, so `cosign verify-attestation --key`
//! (and `oras discover`) read it back.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use oci_client::Reference;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tracing::info;
use umf_oci::image::{ArtifactBlob, emit_artifact_manifest, subject_from_entry};
use umf_oci::registry::{ImageLayout, RegistryError};

use crate::cli::SignKeyType;
use crate::cli::build::{parse_secret_spec, read_secret_material};
use crate::cli::sign::{CliSignError, sign_payload};
use crate::cli::util::{self, CredentialError};

/// in-toto Statement type (the envelope payload).
const IN_TOTO_STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
/// DSSE `payloadType` for an in-toto statement.
const IN_TOTO_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
/// Media type of the DSSE envelope blob.
const DSSE_MEDIA_TYPE: &str = "application/vnd.dsse.envelope.v1+json";
/// cosign referrer artifactType (shared with signatures; the blob media type
/// distinguishes a DSSE attestation from a simple-signing signature).
const COSIGN_SIG_ARTIFACT_TYPE: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";

#[derive(Debug, Error)]
pub(crate) enum CliAttestError {
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
    #[error("predicate file {path}: {err}")]
    PredicateRead { path: String, err: std::io::Error },
    #[error("predicate is not valid JSON: {0}")]
    PredicateJson(serde_json::Error),
    #[error("signing: {0}")]
    Sign(#[from] CliSignError),
    #[error("statement / envelope JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl From<CredentialError> for CliAttestError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf attest` flags.
pub(crate) struct AttestArgs<'a> {
    /// Image to attest (must be present in the local layout).
    pub(crate) reference: &'a str,
    /// Path to the predicate JSON document.
    pub(crate) predicate: &'a Path,
    /// Predicate type: a cosign shorthand (`slsaprovenance`, `spdx`,
    /// `cyclonedx`, …) or a full URI.
    pub(crate) predicate_type: &'a str,
    /// Signing-key spec, same grammar as `umf build --secret`.
    pub(crate) key: &'a str,
    /// Key algorithm; auto-detected from the key when `None`.
    pub(crate) key_type: Option<SignKeyType>,
    pub(crate) push: bool,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
}

/// Sign an in-toto attestation over `reference` and attach the DSSE envelope
/// as a referrer.
pub(crate) fn run_attest(args: AttestArgs<'_>) -> Result<(), CliAttestError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliAttestError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;

    let subject_entry = layout
        .lookup_ref(args.reference)?
        .ok_or_else(|| CliAttestError::ImageNotFound(args.reference.to_string()))?;
    let reference: Reference = args
        .reference
        .parse()
        .map_err(|err: oci_client::ParseError| CliAttestError::BadReference {
            reference: args.reference.to_string(),
            err,
        })?;

    // Key + predicate.
    let spec = parse_secret_spec(args.key).map_err(CliAttestError::KeySpec)?;
    let key_bytes = read_secret_material(&spec).map_err(CliAttestError::KeyRead)?;
    let key_pem = std::str::from_utf8(&key_bytes).map_err(|_| CliAttestError::KeyNotPem)?;
    let predicate_bytes =
        std::fs::read(args.predicate).map_err(|err| CliAttestError::PredicateRead {
            path: args.predicate.display().to_string(),
            err,
        })?;
    let predicate: Value =
        serde_json::from_slice(&predicate_bytes).map_err(CliAttestError::PredicateJson)?;
    let predicate_type = predicate_type_uri(args.predicate_type);

    // in-toto Statement over the subject manifest digest.
    let digest_hex = subject_entry
        .digest
        .strip_prefix("sha256:")
        .unwrap_or(&subject_entry.digest)
        .to_string();
    let docker_reference = format!("{}/{}", reference.registry(), reference.repository());
    let statement = Statement {
        kind: IN_TOTO_STATEMENT_TYPE,
        subject: vec![Subject {
            name: docker_reference,
            digest: BTreeMap::from([("sha256".to_string(), digest_hex)]),
        }],
        predicate_type: predicate_type.clone(),
        predicate,
    };
    let statement_bytes = serde_json::to_vec(&statement)?;

    // DSSE: sign the Pre-Authentication Encoding, wrap the envelope.
    let pae = dsse_pae(IN_TOTO_PAYLOAD_TYPE, &statement_bytes);
    let (_algo, signature) = sign_payload(key_pem, &pae, args.key_type)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let envelope = Dsse {
        payload_type: IN_TOTO_PAYLOAD_TYPE,
        payload: b64.encode(&statement_bytes),
        signatures: vec![DsseSignature {
            sig: b64.encode(&signature),
        }],
    };
    let envelope_bytes = serde_json::to_vec(&envelope)?;

    // Attach as a cosign-shaped referrer.
    let subject = subject_from_entry(&subject_entry);
    let blob = ArtifactBlob {
        media_type: DSSE_MEDIA_TYPE.to_string(),
        data: bytes::Bytes::from(envelope_bytes),
        annotations: None,
    };
    // predicateType rides on the manifest annotations so it surfaces in a
    // referrers listing — how a verifier filters attestations by type.
    let mut manifest_annotations = BTreeMap::new();
    manifest_annotations.insert("predicateType".to_string(), predicate_type.clone());
    let entry = emit_artifact_manifest(
        &layout,
        COSIGN_SIG_ARTIFACT_TYPE,
        Some(&subject),
        std::slice::from_ref(&blob),
        Some(&manifest_annotations),
        None,
    )?;
    info!(subject = %subject.digest, attestation = %entry.digest, predicate_type = %predicate_type, "attested image");
    println!(
        "Attested {subject} ({ptype}) -> attestation {att}",
        subject = subject.digest,
        ptype = predicate_type,
        att = entry.digest,
    );

    if args.push {
        util::push_referrer_for::<CliAttestError>(
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

/// Resolve a predicate-type shorthand to its URI, passing a full URI through.
fn predicate_type_uri(input: &str) -> String {
    match input {
        "slsaprovenance" => "https://slsa.dev/provenance/v1".to_string(),
        "slsaprovenance02" => "https://slsa.dev/provenance/v0.2".to_string(),
        "spdx" => "https://spdx.dev/Document".to_string(),
        "cyclonedx" => "https://cyclonedx.org/bom".to_string(),
        "link" => "https://in-toto.io/Link/v1".to_string(),
        "vuln" => "https://cosign.sigstore.dev/attestation/vuln/v1".to_string(),
        uri => uri.to_string(),
    }
}

/// DSSE Pre-Authentication Encoding (what gets signed):
/// `"DSSEv1" SP len(type) SP type SP len(body) SP body`.
fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut pae = Vec::new();
    pae.extend_from_slice(b"DSSEv1 ");
    pae.extend_from_slice(payload_type.len().to_string().as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload_type.as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload.len().to_string().as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload);
    pae
}

// ── in-toto Statement + DSSE envelope ─────────────────────────────────────

#[derive(Serialize)]
struct Statement {
    #[serde(rename = "_type")]
    kind: &'static str,
    subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    predicate_type: String,
    predicate: Value,
}
#[derive(Serialize)]
struct Subject {
    name: String,
    digest: BTreeMap<String, String>,
}
#[derive(Serialize)]
struct Dsse {
    #[serde(rename = "payloadType")]
    payload_type: &'static str,
    payload: String,
    signatures: Vec<DsseSignature>,
}
#[derive(Serialize)]
struct DsseSignature {
    sig: String,
}

#[cfg(test)]
mod tests;
