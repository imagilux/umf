//! Build-time secret inputs and their resolution into host-side paths.
//!
//! `RUN --mount=type=secret,id=<id>` steps look up the resolved path for
//! their `id` at execution time; the materialised bytes never enter a
//! layer and the secret content never contaminates the cache key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::EngineBuildError;

/// A build-time secret: identified by an id and sourced from a file
/// or an environment variable. Matched at execution time against the
/// `id` on a `RUN --mount=type=secret,id=<id>` mount.
#[derive(Debug, Clone)]
pub struct SecretInput {
    /// Id matched against the `RUN --mount=type=secret,id=<id>` field.
    pub id: String,
    /// Where to read the secret's content from at build time.
    pub source: SecretSource,
}

/// Where a [`SecretInput`]'s bytes come from.
#[derive(Debug, Clone)]
pub enum SecretSource {
    /// Path to a file on the host.
    File(PathBuf),
    /// Name of an environment variable whose value is the secret.
    Env {
        /// The env-var name to read.
        name: String,
    },
}

/// Materialised, build-lifetime bag of secret bytes.
///
/// File-sourced secrets stay where they are on disk; env-sourced secrets
/// get written to a `tempfile::NamedTempFile` whose drop guard lives
/// here so the file vanishes when the build ends. Either way the
/// host-side path is stable for the build's lifetime.
pub(crate) struct ResolvedSecrets {
    /// Drop guards for env-sourced tempfiles. Empty when no `--secret
    /// id=…,env=…` was supplied.
    _tempfiles: Vec<tempfile::NamedTempFile>,
    /// `id → host_path` lookup. `host_path` is either the file source's
    /// original path or the env-tempfile's path.
    by_id: BTreeMap<String, PathBuf>,
}

impl ResolvedSecrets {
    pub(crate) fn host_path_for(&self, id: &str) -> Option<&Path> {
        self.by_id.get(id).map(PathBuf::as_path)
    }
}

/// Resolve every secret input into a host-side path the engine can
/// bind-mount. Env-sourced secrets land in a `NamedTempFile` whose
/// permissions are 0600 (owner-only); file-sourced secrets are used
/// in place (the caller is responsible for the file's permissions).
pub(crate) fn resolve_secrets(inputs: &[SecretInput]) -> Result<ResolvedSecrets, EngineBuildError> {
    let mut tempfiles: Vec<tempfile::NamedTempFile> = Vec::with_capacity(inputs.len());
    let mut by_id: BTreeMap<String, PathBuf> = BTreeMap::new();
    for input in inputs {
        match &input.source {
            SecretSource::File(p) => {
                if !p.exists() {
                    return Err(EngineBuildError::SecretResolution {
                        id: input.id.clone(),
                        reason: format!("source file `{}` does not exist", p.display()),
                    });
                }
                by_id.insert(input.id.clone(), p.clone());
            }
            SecretSource::Env { name } => {
                let value =
                    std::env::var(name).map_err(|_| EngineBuildError::SecretResolution {
                        id: input.id.clone(),
                        reason: format!("environment variable `{name}` is not set"),
                    })?;
                let mut tf = tempfile::NamedTempFile::new()?;
                use std::io::Write as _;
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(tf.path(), std::fs::Permissions::from_mode(0o600))?;
                tf.write_all(value.as_bytes())?;
                tf.flush()?;
                by_id.insert(input.id.clone(), tf.path().to_path_buf());
                tempfiles.push(tf);
            }
        }
    }
    Ok(ResolvedSecrets {
        _tempfiles: tempfiles,
        by_id,
    })
}
