//! Errors produced by the registry client and the on-disk image-layout cache.

use thiserror::Error;

/// Errors produced by the registry client and the on-disk image-layout cache.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Underlying OCI distribution transport error.
    #[error("OCI distribution: {0}")]
    Distribution(#[from] oci_client::errors::OciDistributionError),

    /// Failed to parse a registry reference (`registry/repo:tag@digest`).
    #[error("invalid reference: {0}")]
    Reference(#[from] oci_client::ParseError),

    /// File-system I/O against the on-disk layout.
    #[error("image layout I/O: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode / decode of an OCI manifest or index document.
    #[error("OCI document: {0}")]
    Json(#[from] serde_json::Error),

    /// A blob's sha256 did not match the expected digest.
    #[error("digest mismatch: expected {expected}, found {found}")]
    DigestMismatch {
        /// The digest declared by the manifest or registry response.
        expected: String,
        /// The digest computed locally over the received bytes.
        found: String,
    },

    /// The on-disk layout failed an OCI image-layout invariant.
    #[error("invalid OCI image layout: {0}")]
    InvalidLayout(String),

    /// The named reference is not present in the local layout's `index.json`.
    #[error("reference not found in layout: {0}")]
    NotFound(String),

    /// A digest was not in the `algo:hex` form required by the spec.
    #[error("malformed digest: {0}")]
    MalformedDigest(String),

    /// A caller-supplied media type (e.g. an artifact manifest's
    /// `artifactType`) was not in the RFC 6838 `type/subtype` form the
    /// image-spec requires.
    #[error("invalid media type: {0:?} (expected RFC 6838 `type/subtype`)")]
    InvalidMediaType(String),

    /// `mkfs.erofs` failed to encode a layer into an erofs image. The
    /// caller is expected to fall back to the unpack path on this error
    /// (erofs is an optional acceleration; see [`crate::erofs`]).
    #[error("erofs encode: {0}")]
    Erofs(String),
}

impl RegistryError {
    /// Whether this error is plausibly a transient transport blip worth
    /// retrying: a connection/IO error or an OCI-distribution transport
    /// failure. Permanent, local, or content errors (digest mismatch, missing
    /// reference, malformed input, JSON / layout errors) return `false`, since
    /// a retry would only repeat the same failure.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Distribution(_) | Self::Io(_))
    }
}
