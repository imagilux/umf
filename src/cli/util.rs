//! Helpers shared across two or more `umf` subcommands: layout-dir
//! resolution, recipe discovery, registry-client construction,
//! credential-override parsing, column truncation, and the byte
//! formatters.

use std::path::{Path, PathBuf};

use oci_client::Reference;
use oci_client::client::ClientProtocol;
use thiserror::Error;
use umf_oci::registry::auth::CredentialOverride;
use umf_oci::registry::{RegistryClient, default_client_config};

/// Recipe filenames searched inside a directory, in precedence order.
/// Mirrors Podman/Buildah discovery — reusing the OCI-standard names
/// (rather than a UMF-specific one) so existing docker/podman source
/// trees build with no rename. A UMF recipe is a superset of these.
const RECIPE_NAMES: &[&str] = &["Containerfile", "Dockerfile"];

/// The outcome of [`resolve_recipe`]: the concrete recipe file to parse
/// plus the build-context directory that relative `ADD` sources and the
/// engine bundle root resolve against.
#[derive(Debug)]
pub(crate) struct ResolvedRecipe {
    /// The recipe source file to read + parse.
    pub(crate) recipe: PathBuf,
    /// The build context directory.
    pub(crate) context: PathBuf,
}

/// Why [`resolve_recipe`] could not locate a recipe. Each CLI handler
/// maps this onto its own error enum (or prints it directly).
#[derive(Debug, Error)]
pub(crate) enum RecipeResolveError {
    #[error("no recipe found in {dir} (looked for {names}); pass -f/--file to point at one")]
    NoneInDir { dir: String, names: String },
    #[error("path does not exist: {0}")]
    NotFound(String),
    #[error("--file {0} does not exist")]
    FileMissing(String),
    #[error("--file {0} is a directory, not a recipe file")]
    FileIsDir(String),
}

/// Resolve the recipe path and build context from a positional path
/// argument and an optional `-f/--file` override, applying Docker-style
/// discovery (`Containerfile` → `Dockerfile` inside a directory).
///
/// - **`--file` given**: that's the recipe verbatim; the context is the
///   positional when it's a directory, else the current directory
///   (Docker treats the positional as the build context under `-f`).
/// - **positional is a file**: it is the recipe; the context is its
///   parent (an empty parent — a bare filename — normalizes to `.`).
/// - **positional is a directory, or absent (defaults to `.`)**: the
///   directory is searched for [`RECIPE_NAMES`]; the first hit is the
///   recipe and the directory is the context.
/// - **positional does not exist**: [`RecipeResolveError::NotFound`].
pub(crate) fn resolve_recipe(
    positional: Option<&Path>,
    file: Option<&Path>,
) -> Result<ResolvedRecipe, RecipeResolveError> {
    if let Some(file) = file {
        if !file.exists() {
            return Err(RecipeResolveError::FileMissing(file.display().to_string()));
        }
        if file.is_dir() {
            return Err(RecipeResolveError::FileIsDir(file.display().to_string()));
        }
        let context = match positional {
            Some(p) if p.is_dir() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        return Ok(ResolvedRecipe {
            recipe: file.to_path_buf(),
            context,
        });
    }

    let target = positional.unwrap_or_else(|| Path::new("."));
    if target.is_dir() {
        for name in RECIPE_NAMES {
            let candidate = target.join(name);
            if candidate.is_file() {
                return Ok(ResolvedRecipe {
                    recipe: candidate,
                    context: target.to_path_buf(),
                });
            }
        }
        return Err(RecipeResolveError::NoneInDir {
            dir: target.display().to_string(),
            names: RECIPE_NAMES.join(", "),
        });
    }
    if target.is_file() {
        let context = match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        return Ok(ResolvedRecipe {
            recipe: target.to_path_buf(),
            context,
        });
    }
    Err(RecipeResolveError::NotFound(target.display().to_string()))
}

/// Resolve the default OCI image-layout directory.
///
/// Prefers `$XDG_CACHE_HOME/umf/oci-layout`, falling back to
/// `~/.cache/umf/oci-layout`. Returns the `$HOME not set` guidance
/// string as the `Err` so each subcommand can wrap it in its own
/// `LayoutDir(String)` error variant.
///
/// Consolidated from the per-subcommand `default_layout_dir*`
/// helpers — the bodies were byte-identical apart from the error
/// type they constructed.
pub(crate) fn default_layout_dir() -> Result<PathBuf, String> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg).join("umf").join("oci-layout"));
    }
    let home = std::env::var("HOME").map_err(|_| "$HOME not set; pass --layout-dir".to_string())?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("umf")
        .join("oci-layout"))
}

/// Failure modes for [`credential_override`]. Neutral across
/// subcommands so each can map it onto its own error enum (every
/// caller carries matching `PasswordStdinWithoutUsername` +
/// `PasswordStdin(io::Error)` variants).
pub(crate) enum CredentialError {
    /// `--password-stdin` was passed without `--username`.
    PasswordStdinWithoutUsername,
    /// Reading the password line from stdin failed.
    PasswordStdin(std::io::Error),
}

/// Translate the `--username` / `--password-stdin` flag pair into a
/// [`CredentialOverride`]. When `--username` is absent the result is the
/// `Default` override (the resolver then falls through to env vars and
/// `~/.docker/config.json`).
///
/// Consolidated from the per-subcommand `*_credential_override`
/// helpers — the bodies were identical apart from the error type.
pub(crate) fn credential_override(
    username: Option<&str>,
    password_stdin: bool,
) -> Result<CredentialOverride, CredentialError> {
    if password_stdin && username.is_none() {
        return Err(CredentialError::PasswordStdinWithoutUsername);
    }
    let Some(user) = username else {
        return Ok(CredentialOverride::default());
    };
    let password = if password_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .map_err(CredentialError::PasswordStdin)?;
        // Strip trailing newline written by interactive shells / `echo`.
        Some(buf.trim_end_matches(['\r', '\n']).to_string())
    } else {
        None
    };
    Ok(CredentialOverride {
        username: Some(user.to_string()),
        password,
    })
}

/// Build a registry client for `reference`, optionally allowing
/// plain-HTTP traffic to that registry (`--insecure-registry`).
pub(crate) fn registry_client_for(reference: &Reference, insecure: bool) -> RegistryClient {
    // Start from UMF's default client config (connect/read timeouts set) and
    // only override the protocol for --insecure-registry, so the timeout
    // ceilings apply on both the secure and insecure paths.
    let mut cfg = default_client_config();
    if insecure {
        let registry = reference.registry().to_string();
        eprintln!(
            "warning: --insecure-registry sends traffic to {registry} over plain HTTP; any \
             credentials and image data cross the wire unencrypted (a `Basic` auth header is \
             base64, not encryption). Prefer HTTPS; use --insecure-registry only anonymously or \
             over a trusted link."
        );
        cfg.protocol = ClientProtocol::HttpsExcept(vec![registry]);
    }
    RegistryClient::with_config(cfg)
}

/// Pull `reference` into `layout` if it isn't already present, using the same
/// `override → docker-config → anonymous` credential chain everywhere else
/// uses. A no-op when the image is already in the layout. Generic over the
/// caller's error type so `umf run` and `umf inspect` share one
/// pull-on-miss implementation.
///
/// # Errors
/// The caller's error `E` (built from a credential, registry, or I/O failure).
pub(crate) fn pull_if_missing<E>(
    layout: &umf_oci::registry::ImageLayout,
    reference: &Reference,
    raw_reference: &str,
    username: Option<&str>,
    password_stdin: bool,
    insecure_registry: bool,
) -> Result<(), E>
where
    E: From<CredentialError> + From<umf_oci::registry::RegistryError> + From<std::io::Error>,
{
    // Presence is "is this ref in the layout's index.json", not "does it
    // introspect cleanly": a multi-arch image index is present-but-not-
    // introspectable (introspect requires a per-arch selection), and a
    // locally-composed index (`umf index`, never pushed) must inspect offline
    // rather than trigger a doomed network pull.
    if layout.lookup_ref(raw_reference).map_err(E::from)?.is_none() {
        tracing::info!(reference = %reference, "image not in layout; pulling");
        let credentials = credential_override(username, password_stdin)?;
        let auth =
            umf_oci::registry::auth::resolve_auth_for(Some(reference.registry()), &credentials);
        let client = registry_client_for(reference, insecure_registry);
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(client.pull(reference, &auth, layout))?;
    }
    Ok(())
}

/// Push an artifact manifest (a referrer, by digest) to `reference`'s
/// registry, maintaining the OCI 1.1 referrers index or its fallback tag.
/// Shared by `umf sbom` and `umf sign`; generic over the caller's error like
/// [`pull_if_missing`], so each maps the credential / registry / I/O failures
/// onto its own enum. The subject image must already be in that registry.
pub(crate) fn push_referrer_for<E>(
    layout: &umf_oci::registry::ImageLayout,
    reference: &Reference,
    username: Option<&str>,
    password_stdin: bool,
    insecure_registry: bool,
    entry: &oci_client::manifest::ImageIndexEntry,
) -> Result<(), E>
where
    E: From<CredentialError> + From<umf_oci::registry::RegistryError> + From<std::io::Error>,
{
    let credentials = credential_override(username, password_stdin)?;
    let auth = umf_oci::registry::auth::resolve_auth_for(Some(reference.registry()), &credentials);
    let client = registry_client_for(reference, insecure_registry);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(client.push_referrer(reference, entry, layout, &auth))?;
    println!("Pushed referrer {} to {}", entry.digest, reference);
    Ok(())
}

/// Truncate `s` to `width` characters, appending an ellipsis when it
/// overflows.
///
/// Counts and slices by `char`, never by byte: a byte-index slice would panic
/// on a multi-byte UTF-8 boundary (a non-ASCII `ref.name` imported via
/// `umf load` reaches the `umf images` table this way).
pub(crate) fn truncate_for_column(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let take = width.saturating_sub(1);
        format!("{}…", s.chars().take(take).collect::<String>())
    }
}

/// Char-safe prefix: the first `max` characters of `s`, cut on a UTF-8
/// boundary (no ellipsis). Unlike `&s[..max]` this never panics on multi-byte
/// input — used to abbreviate digests that, after `umf load` of a crafted
/// archive or a hostile registry response, are not guaranteed to be ASCII.
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Right-padded binary-unit byte formatter for the `umf inspect` table
/// (`{:>6.2}` so the layer column stays aligned; sub-KiB falls back to
/// a padded raw byte count).
pub(crate) fn human_size(bytes: i64) -> String {
    let bytes = bytes.max(0) as f64;
    const KI: f64 = 1024.0;
    const MI: f64 = KI * 1024.0;
    const GI: f64 = MI * 1024.0;
    if bytes >= GI {
        format!("{:>6.2} Gi", bytes / GI)
    } else if bytes >= MI {
        format!("{:>6.2} Mi", bytes / MI)
    } else if bytes >= KI {
        format!("{:>6.2} Ki", bytes / KI)
    } else {
        format!("{bytes:>6}  ")
    }
}

/// Binary-unit byte formatter for the `umf images` table + prune
/// report. Unpadded `{:.2}`; sub-KiB renders as `<n> B`. Kept distinct
/// from [`human_size`] because the two columns use different padding /
/// sub-KiB shapes (both are snapshot-tested).
pub(crate) fn layout_human_bytes(bytes: i64) -> String {
    layout_human_bytes_u64(bytes.max(0) as u64)
}

/// Unsigned variant of [`layout_human_bytes`] for the prune report's
/// freed-byte total.
pub(crate) fn layout_human_bytes_u64(bytes: u64) -> String {
    let b = bytes as f64;
    const KI: f64 = 1024.0;
    const MI: f64 = KI * 1024.0;
    const GI: f64 = MI * 1024.0;
    if b >= GI {
        format!("{:.2} Gi", b / GI)
    } else if b >= MI {
        format!("{:.2} Mi", b / MI)
    } else if b >= KI {
        format!("{:.2} Ki", b / KI)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests;
