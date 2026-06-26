//! Registry credential resolution.
//!
//! Replaces hardcoded [`RegistryAuth::Anonymous`] call sites with a layered
//! lookup that reads from (in order, highest priority first):
//!
//! 1. **Explicit overrides** — passed in by the caller (e.g. `--username`
//!    + `--password-stdin` on the CLI).
//! 2. **Environment variables** — `UMF_REGISTRY_USERNAME` /
//!    `UMF_REGISTRY_PASSWORD`. Apply to every host this invocation
//!    touches; use the registry-auth config file (next layer) for
//!    per-host credentials.
//! 3. **Registry-auth config file** — `$DOCKER_CONFIG/config.json` if
//!    set, otherwise `~/.docker/config.json`. This file is the
//!    de-facto OCI ecosystem credential store: containerd, podman,
//!    buildah, skopeo all read the same shape. Within the file the
//!    precedence is: a per-host `credHelpers` entry (execs
//!    `docker-credential-<name>`), then the global `credsStore`
//!    helper, then the inline `auths[<host>]` entry (base64 `auth` or
//!    `username` + `password`). A configured helper is authoritative
//!    for its host — helper failure degrades to anonymous with one
//!    warning rather than silently using a stale inline secret, which
//!    is `docker login`'s own semantic (with a store configured, the
//!    inline entry is an empty marker). Falls back to the Docker Hub
//!    aliases for `docker.io` / `registry-1.docker.io` since that's
//!    the convention everyone follows.
//! 4. **Anonymous** — same default the codebase had before this
//!    resolver existed; the resolver never returns an error, only a
//!    less-privileged auth value.
//!
//! Credential-helper protocol (`docker-credential-<name> get`): the
//! registry host goes to the child's stdin, the reply is JSON
//! `{"Username": …, "Secret": …}` on stdout; the `<token>` username
//! convention maps to [`RegistryAuth::Bearer`]. Helper names are
//! validated to `[A-Za-z0-9._-]+` (no path separators, executed
//! without a shell), replies are size-capped, and the secret never
//! reaches logs or traces.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use tracing::{debug, warn};

use oci_client::secrets::RegistryAuth;

/// Username + password pair supplied out-of-band (e.g. CLI flags).
///
/// Highest-priority input to [`resolve_auth_for`].
#[derive(Clone, Default)]
pub struct CredentialOverride {
    /// Explicit username; `None` means "fall through to the next layer".
    pub username: Option<String>,
    /// Explicit password; `None` means "fall through to the next layer".
    pub password: Option<String>,
}

// Manual `Debug` that never prints the password value — only whether one is
// present — so an accidental `{:?}` / `dbg!` / tracing field / panic can't leak
// the credential. (The username is not secret and is kept for diagnostics.)
impl std::fmt::Debug for CredentialOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialOverride")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl CredentialOverride {
    /// Returns a [`RegistryAuth::Basic`] if both fields are set; `None`
    /// otherwise.
    fn as_basic(&self) -> Option<RegistryAuth> {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => Some(RegistryAuth::Basic(u.clone(), p.clone())),
            _ => None,
        }
    }
}

/// Resolve the [`RegistryAuth`] for `host`.
///
/// `host` is the registry host extracted from the reference (e.g.
/// `registry.bitswalk.net`, `quay.io`). When the reference carries
/// no explicit host, pass [`None`] and the resolver will treat it as
/// Docker Hub (`docker.io`) — the de-facto default registry for
/// short-form refs like `alpine:3.21`.
///
/// Never returns an error: every lookup layer falls through to
/// [`RegistryAuth::Anonymous`] if nothing matches. Errors reading
/// the on-disk auth config are logged at `warn` level and swallowed.
#[must_use]
pub fn resolve_auth_for(host: Option<&str>, override_: &CredentialOverride) -> RegistryAuth {
    if let Some(auth) = override_.as_basic() {
        debug!(host = ?host, "registry auth: using explicit override");
        return auth;
    }
    if let Some(auth) = env_auth() {
        debug!(host = ?host, "registry auth: using UMF_REGISTRY_* env vars");
        return auth;
    }
    if let Some(auth) = config_file_auth_at(config_file_path(), host) {
        debug!(host = ?host, "registry auth: using ~/.docker/config.json");
        return auth;
    }
    debug!(host = ?host, "registry auth: anonymous (no credentials found)");
    RegistryAuth::Anonymous
}

// ── Env-var layer ───────────────────────────────────────────────────────────

fn env_auth() -> Option<RegistryAuth> {
    let user = env::var("UMF_REGISTRY_USERNAME").ok()?;
    let pass = env::var("UMF_REGISTRY_PASSWORD").ok()?;
    if user.is_empty() || pass.is_empty() {
        return None;
    }
    Some(RegistryAuth::Basic(user, pass))
}

// ── On-disk auth-config layer ───────────────────────────────────────────────
//
// The file is the de-facto OCI ecosystem credential store —
// `~/.docker/config.json` historically, also read verbatim by
// containerd, podman, buildah, skopeo. The type names below reflect
// what the JSON file actually represents to us (a registry auth
// config), not the file's lineage.

/// Subset of `~/.docker/config.json` that we care about.
#[derive(Debug, Deserialize, Default)]
struct AuthConfigFile {
    #[serde(default)]
    auths: std::collections::BTreeMap<String, AuthEntry>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

fn config_file_path() -> Option<PathBuf> {
    if let Ok(dir) = env::var("DOCKER_CONFIG") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("config.json"));
        }
    }
    let home = env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".docker").join("config.json"))
}

fn config_file_auth_at(path: Option<PathBuf>, host: Option<&str>) -> Option<RegistryAuth> {
    let path = path?;
    if !path.is_file() {
        return None;
    }
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(err) => {
            warn!(?err, path = %path.display(), "failed to read registry auth config");
            return None;
        }
    };
    let config: AuthConfigFile = match serde_json::from_str(&body) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, path = %path.display(), "failed to parse registry auth config");
            return None;
        }
    };

    let key = pick_auth_key(&config, host)?;

    // Per-host credential helper first, then the global store. Either
    // being configured is authoritative for this layer: a helper that
    // fails (missing binary, no stored credential, malformed reply)
    // degrades to anonymous with one warning, mirroring docker — with a
    // store configured the inline entry is an empty marker, so falling
    // back to it would mean using a stale or empty secret.
    if let Some(helper) = config.cred_helpers.get(&key) {
        return helper_auth(helper, &key);
    }
    if let Some(store) = &config.creds_store {
        return helper_auth(store, &key);
    }

    let entry = config.auths.get(&key)?;
    if let Some(b64) = &entry.auth {
        if let Some(auth) = parse_b64_auth(b64) {
            return Some(auth);
        }
    }
    if let (Some(u), Some(p)) = (&entry.username, &entry.password) {
        return Some(RegistryAuth::Basic(u.clone(), p.clone()));
    }
    None
}

// ── Credential-helper layer ─────────────────────────────────────────────────
//
// `docker-credential-<name> get` — the protocol shared by every Docker /
// containerd / podman credential helper: server key on stdin, JSON reply
// on stdout. The secret is never logged; failures degrade to `None`.

/// Ceiling for a helper's stdout reply. Real replies are a few hundred
/// bytes of JSON; the cap bounds a misbehaving helper.
const MAX_HELPER_REPLY_BYTES: u64 = 64 * 1024;

/// The JSON document a credential helper prints for `get`. `ServerURL`
/// (and anything else) is ignored.
#[derive(Deserialize)]
struct HelperReply {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

/// Run the credential helper `helper` for `host` and convert its reply.
///
/// Validates the helper name, execs `docker-credential-<helper>` (resolved
/// via `PATH`, never through a shell), and maps the `<token>` username
/// convention to [`RegistryAuth::Bearer`]. Every failure path warns once
/// — without the reply or secret — and returns `None`.
fn helper_auth(helper: &str, host: &str) -> Option<RegistryAuth> {
    if !helper_name_is_valid(helper) {
        warn!(
            helper = %helper,
            "credential helper name is not [A-Za-z0-9._-]+; refusing to execute"
        );
        return None;
    }
    helper_auth_via(Command::new(format!("docker-credential-{helper}")), host)
}

/// `true` when `helper` is a plain helper suffix — ASCII alphanumerics
/// plus `.`/`_`/`-` — so the executed program name can't smuggle a path
/// or shell metacharacters.
fn helper_name_is_valid(helper: &str) -> bool {
    !helper.is_empty()
        && helper
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Drive an already-constructed helper [`Command`] through the `get`
/// protocol. Split from [`helper_auth`] so tests can point the command at
/// a fixture script by absolute path instead of mutating `PATH`.
fn helper_auth_via(mut cmd: Command, host: &str) -> Option<RegistryAuth> {
    use std::io::{Read as _, Write as _};

    cmd.arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!(?err, host = %host, "credential helper failed to spawn; falling back to anonymous");
            return None;
        }
    };

    // The host key is tiny, so this write can't fill the pipe; helpers
    // read stdin to EOF before replying.
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(host.as_bytes()) {
            warn!(?err, host = %host, "failed to hand the host to the credential helper");
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }

    let mut reply = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        // +1 so an at-cap read is distinguishable from an over-cap one.
        if let Err(err) = stdout
            .take(MAX_HELPER_REPLY_BYTES + 1)
            .read_to_end(&mut reply)
        {
            warn!(?err, host = %host, "failed to read the credential helper reply");
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            warn!(?err, host = %host, "credential helper did not exit cleanly");
            return None;
        }
    };

    helper_reply_auth(status.success(), host, &reply)
}

/// Convert a finished helper invocation — exit success plus the (capped)
/// stdout reply — into an auth value. Pure: no process machinery, so every
/// acceptance and rejection branch is unit-testable in-process.
fn helper_reply_auth(exit_ok: bool, host: &str, reply: &[u8]) -> Option<RegistryAuth> {
    if !exit_ok {
        // The standard helpers exit 1 with "credentials not found in
        // native keychain" — an expected miss, not an error.
        warn!(host = %host, "credential helper returned no credentials; falling back to anonymous");
        return None;
    }
    if reply.len() as u64 > MAX_HELPER_REPLY_BYTES {
        warn!(host = %host, "credential helper reply exceeds the size cap; ignoring it");
        return None;
    }
    let parsed: HelperReply = match serde_json::from_slice(reply) {
        Ok(parsed) => parsed,
        Err(_) => {
            // Deliberately drop the serde error: it can embed reply text.
            warn!(host = %host, "credential helper reply is not the expected JSON shape");
            return None;
        }
    };
    if parsed.username.is_empty() || parsed.secret.is_empty() {
        warn!(host = %host, "credential helper reply is missing a username or secret");
        return None;
    }
    // The `<token>` username is the helper convention for an identity
    // token (OAuth-style registries); everything else is basic auth.
    if parsed.username == "<token>" {
        return Some(RegistryAuth::Bearer(parsed.secret));
    }
    Some(RegistryAuth::Basic(parsed.username, parsed.secret))
}

/// Pick the entry key from the auth config that best matches `host`.
///
/// Tries (in order): the host verbatim, `https://<host>`, `https://<host>/v1/`,
/// `http://<host>`. For Docker Hub (`docker.io`, `registry-1.docker.io`,
/// `index.docker.io`) also tries the historical `https://index.docker.io/v1/`
/// key that `docker login` writes by default.
fn pick_auth_key(config: &AuthConfigFile, host: Option<&str>) -> Option<String> {
    let host = host.unwrap_or("docker.io");
    let candidates: Vec<String> = if is_docker_hub(host) {
        vec![
            "https://index.docker.io/v1/".to_string(),
            "index.docker.io".to_string(),
            "registry-1.docker.io".to_string(),
            "docker.io".to_string(),
            host.to_string(),
        ]
    } else {
        vec![
            host.to_string(),
            format!("https://{host}"),
            format!("https://{host}/v1/"),
            format!("http://{host}"),
        ]
    };
    for key in &candidates {
        if config.auths.contains_key(key) || config.cred_helpers.contains_key(key) {
            return Some(key.clone());
        }
    }
    // Final fallback: when only a global `credsStore` is configured, return
    // the canonical host so the caller can emit the warning above.
    if config.creds_store.is_some() {
        return Some(
            candidates
                .into_iter()
                .next()
                .unwrap_or_else(|| host.to_string()),
        );
    }
    None
}

const fn is_docker_hub(host: &str) -> bool {
    matches!(
        host.as_bytes(),
        b"docker.io" | b"registry-1.docker.io" | b"index.docker.io"
    )
}

// ── Base64 decode for the `auth` field ──────────────────────────────────────
//
// The `auth` field is the standard-alphabet base64 of `username:password`,
// decoded via the `base64` crate's standard engine.

fn parse_b64_auth(b64: &str) -> Option<RegistryAuth> {
    let decoded = base64_decode(b64.trim())?;
    let s = std::str::from_utf8(&decoded).ok()?;
    let (u, p) = s.split_once(':')?;
    if u.is_empty() {
        return None;
    }
    Some(RegistryAuth::Basic(u.to_string(), p.to_string()))
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    STANDARD.decode(input).ok()
}

#[cfg(test)]
mod tests;
