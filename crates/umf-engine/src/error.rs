//! Errors produced by `umf-engine`.

use thiserror::Error;
use umf_oci::registry::error::RegistryError;

/// Single public error type for the engine.
///
/// Backends map their native errors into [`EngineError::Runtime`]; bundle
/// preparation and overlay setup produce the more specific variants.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The image referenced for bundle preparation isn't in the layout.
    #[error("image `{0}` not in the OCI layout")]
    ImageNotInLayout(String),

    /// The image resolved to a multi-arch index that carries no manifest
    /// for the requested target architecture. We refuse to fall back to an
    /// arbitrary arch so a cross-arch (`--platform`) bundle can't silently
    /// unpack the wrong rootfs.
    #[error(
        "no manifest for platform linux/{arch} in the image index \
        (the image does not publish that architecture)"
    )]
    NoManifestForPlatform {
        /// OCI-shorthand arch that was requested (`amd64` / `arm64`).
        arch: String,
    },

    /// A [`crate::BuildHook`] returned [`crate::HookAction::Abort`]
    /// from `before_step`; the build was stopped cleanly without
    /// executing the pending directive. Used by `umf debug build`'s
    /// `quit` command.
    #[error("build aborted by hook at stage {stage_index}, step {step_index}")]
    BuildAborted {
        /// 1-based stage index where the abort happened.
        stage_index: usize,
        /// 1-based step index within the stage.
        step_index: u32,
    },

    /// The pulled image's manifest had a layer count that didn't match the
    /// number of `diff_ids` in its image-config.
    #[error("manifest / config disagree: {layers} layers vs {diff_ids} diff_ids")]
    LayerCountMismatch {
        /// Number of layer descriptors on the manifest.
        layers: usize,
        /// Number of `rootfs.diff_ids` entries on the image-config.
        diff_ids: usize,
    },

    /// A layer's media type isn't one we know how to unpack (we
    /// expect `application/vnd.oci.image.layer.v1.tar+gzip` or its
    /// legacy `application/vnd.docker.image.rootfs.diff.tar.gzip`
    /// equivalent — both are gzipped tar layers).
    #[error("unsupported layer media-type: {0}")]
    UnsupportedLayerMediaType(String),

    /// The OCI runtime spec we constructed couldn't be serialised to JSON.
    /// Should never happen with well-typed inputs — surfaced as a hard
    /// error rather than silently dropped.
    #[error("could not serialise runtime spec: {0}")]
    SerialiseSpec(#[source] serde_json::Error),

    /// Filesystem error during bundle prep, overlay setup, or upper-dir
    /// snapshot capture.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode/decode of an OCI document inside the layout.
    #[error("OCI document: {0}")]
    Json(#[from] serde_json::Error),

    /// The on-disk OCI layout produced a registry-side error.
    #[error("OCI layout: {0}")]
    Registry(#[from] RegistryError),

    /// Backend-side runtime error: anything that went wrong during the
    /// actual container execution. Backends populate this with a
    /// concrete display string so the caller can surface it; the
    /// underlying error chain is preserved.
    #[error("runtime backend: {message}")]
    Runtime {
        /// Human-readable summary.
        message: String,
        /// Optional underlying cause.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
}

impl EngineError {
    /// Construct a [`EngineError::Runtime`] from a message + optional cause.
    #[must_use]
    pub fn runtime(
        message: impl Into<String>,
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self::Runtime {
            message: message.into(),
            source,
        }
    }
}
