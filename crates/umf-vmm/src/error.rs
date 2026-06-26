//! Errors produced by `umf-vmm`.

use std::path::PathBuf;

use thiserror::Error;

/// Single public error type for `umf-vmm`. Backends map their native
/// errors into the appropriate variant; the [`Self::Backend`] catch-all
/// preserves the underlying error chain for diagnostics.
#[derive(Debug, Error)]
pub enum VmError {
    /// Required VMM binary isn't on `PATH`. The variant carries the
    /// looked-for binary name so the caller can surface a targeted
    /// install hint (`qemu-system-aarch64 not found — apt install
    /// qemu-system-arm`).
    #[error("VMM binary `{0}` not found on PATH")]
    BinaryNotFound(String),

    /// Disk image / firmware / other input file referenced by [`crate::VmSpec`]
    /// doesn't exist or isn't readable.
    #[error("VMM input file {path} unusable: {reason}")]
    InputUnusable {
        /// Path the spec referenced.
        path: PathBuf,
        /// Human-readable reason (file missing, permission denied, ...).
        reason: String,
    },

    /// The VMM started but failed before reaching the running state
    /// (typically: bad disk image, missing firmware, KVM denied).
    #[error("VMM failed to boot: {0}")]
    BootFailed(String),

    /// QMP / REST control channel error from a backend that exposes one.
    /// Surfaced when the channel disconnects mid-command or the VMM
    /// returns an explicit error response.
    #[error("VMM control channel: {0}")]
    Control(String),

    /// The VMM process exited abnormally (segfault / SIGKILL / similar)
    /// rather than via the requested shutdown path.
    #[error("VMM process exited abnormally with status {status:?}")]
    AbnormalExit {
        /// Exit status the underlying process reported.
        status: std::process::ExitStatus,
    },

    /// Graceful shutdown (ACPI / `system_powerdown` / `vm.shutdown`) was
    /// issued but the VMM didn't exit within the deadline. Callers can
    /// retry with `graceful=false` to issue a hard kill.
    #[error("graceful shutdown timed out after {timeout:?}")]
    ShutdownTimeout {
        /// The deadline that was exceeded.
        timeout: std::time::Duration,
    },

    /// Filesystem I/O error during spawn (socket path creation, log file
    /// open, ...).
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all for backend-specific errors. Backends populate this
    /// with a concrete display string + the underlying source so the
    /// caller can both display a summary and walk the cause chain.
    #[error("VMM backend: {message}")]
    Backend {
        /// Human-readable summary the CLI surfaces.
        message: String,
        /// Optional underlying cause.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
}

impl VmError {
    /// Construct a [`Self::Backend`] from a message + optional cause.
    /// Sugar so backends don't have to spell out the struct fields at
    /// every error site.
    #[must_use]
    pub fn backend(
        message: impl Into<String>,
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self::Backend {
            message: message.into(),
            source,
        }
    }
}
