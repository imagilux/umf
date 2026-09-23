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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    // The workspace denies `unsafe_code`, and `std::env::set_var` is unsafe in
    // edition 2024, so these tests never set an environment variable. `PATH` is
    // present in every process and serves as a real env-sourced secret whose
    // expected value the test can read safely.
    const ALWAYS_SET: &str = "PATH";
    const NEVER_SET: &str = "UMF_TEST_SECRET_THAT_IS_NEVER_SET_7F3A9C";

    fn env_secret(id: &str, name: &str) -> SecretInput {
        SecretInput {
            id: id.to_string(),
            source: SecretSource::Env {
                name: name.to_string(),
            },
        }
    }

    /// An env-sourced secret is written to a file only its owner can read.
    ///
    /// The tempfile exists so a `RUN` step can bind-mount it. If it were
    /// world- or group-readable, any other local user could read the secret
    /// for the lifetime of the build — a signing key, a registry token.
    #[test]
    fn an_env_sourced_secret_is_written_owner_only() {
        let resolved = resolve_secrets(&[env_secret("tok", ALWAYS_SET)]).expect("resolve");
        let path = resolved.host_path_for("tok").expect("resolved path");

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only, got {mode:o}");

        let expected = std::env::var(ALWAYS_SET).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
    }

    /// The env-sourced tempfile is removed when the build's secrets drop.
    ///
    /// The module promises the file "vanishes when the build ends". If it
    /// outlived the build, every build that used an env secret would leave a
    /// copy of it in the system temp directory indefinitely.
    #[test]
    fn an_env_sourced_secret_file_is_removed_when_the_build_ends() {
        let resolved = resolve_secrets(&[env_secret("tok", ALWAYS_SET)]).expect("resolve");
        let path = resolved.host_path_for("tok").unwrap().to_path_buf();
        assert!(
            path.exists(),
            "precondition: the secret file exists mid-build"
        );

        drop(resolved);
        assert!(
            !path.exists(),
            "the secret file outlived the build: {}",
            path.display(),
        );
    }

    /// A file-sourced secret is used in place — never copied anywhere the
    /// build would then have to remember to clean up.
    #[test]
    fn a_file_sourced_secret_is_used_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("key.pem");
        std::fs::write(&src, b"-----BEGIN KEY-----").unwrap();

        let resolved = resolve_secrets(&[SecretInput {
            id: "signing".into(),
            source: SecretSource::File(src.clone()),
        }])
        .expect("resolve");
        assert_eq!(resolved.host_path_for("signing"), Some(src.as_path()));
    }

    #[test]
    fn a_missing_secret_file_is_an_error_naming_the_id() {
        let err = resolve_secrets(&[SecretInput {
            id: "signing".into(),
            source: SecretSource::File("/nonexistent/umf/key.pem".into()),
        }])
        .err()
        .expect("a missing source file must be an error");
        assert!(
            matches!(&err, EngineBuildError::SecretResolution { id, .. } if id == "signing"),
            "expected SecretResolution naming `signing`, got {err:?}",
        );
    }

    #[test]
    fn an_unset_secret_env_var_is_an_error_naming_the_id() {
        let err = resolve_secrets(&[env_secret("tok", NEVER_SET)])
            .err()
            .expect("an unset source variable must be an error");
        assert!(
            matches!(&err, EngineBuildError::SecretResolution { id, .. } if id == "tok"),
            "expected SecretResolution naming `tok`, got {err:?}",
        );
    }

    /// An id no `--secret` supplied resolves to nothing, so the `RUN` step
    /// fails with `MissingSecret` rather than mounting something unintended.
    #[test]
    fn an_unknown_secret_id_resolves_to_nothing() {
        let resolved = resolve_secrets(&[env_secret("tok", ALWAYS_SET)]).expect("resolve");
        assert!(resolved.host_path_for("other").is_none());
    }
}
