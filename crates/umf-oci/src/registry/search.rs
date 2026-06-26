//! Operator-configured **search registries** for unqualified references.
//!
//! A bare reference like `alpine:3.23` carries no registry host, so by default
//! it resolves against Docker Hub (`docker.io`). This module lets an operator
//! register additional registries (via `umf registry`) that UMF tries first,
//! **in order**, before falling back to Docker Hub. It mirrors Podman's
//! `unqualified-search-registries` model. A fully-qualified reference (one with
//! an explicit host) is never rewritten.
//!
//! The list is persisted as TOML at `$XDG_CONFIG_HOME/umf/registries.toml`
//! (default `~/.config/umf/registries.toml`):
//!
//! ```toml
//! search = ["registry.example.com", "ghcr.io"]
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The operator's ordered list of registries searched for an unqualified
/// reference, before the implicit `docker.io` fallback.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRegistries {
    /// Registry hosts tried in order for a bare reference. Empty by default,
    /// which preserves the Docker-Hub-only behaviour.
    #[serde(default)]
    pub search: Vec<String>,
}

impl SearchRegistries {
    /// Path to the config file: `$XDG_CONFIG_HOME/umf/registries.toml`, else
    /// `~/.config/umf/registries.toml`. `None` when neither `XDG_CONFIG_HOME`
    /// nor `HOME` is set.
    #[must_use]
    pub fn config_path() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("umf").join("registries.toml"));
        }
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("umf")
                .join("registries.toml")
        })
    }

    /// Load the configured search registries from the default config path. A
    /// missing file (or no resolvable path) is an empty list, never an error,
    /// so resolution always has a sane default.
    #[must_use]
    pub fn load() -> Self {
        Self::config_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    /// Load from an explicit path. A missing file is the default (empty); a
    /// present-but-unparseable file is logged and treated as empty so a typo
    /// never blocks a build.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(), error = %e,
                    "umf registries.toml is unparseable; ignoring it"
                );
                Self::default()
            }
        }
    }

    /// Persist to the default config path, creating parent directories.
    ///
    /// # Errors
    /// I/O errors, or no config path resolves (`$HOME` / `$XDG_CONFIG_HOME`
    /// both unset).
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no config path: set HOME or XDG_CONFIG_HOME",
            )
        })?;
        self.save_to(&path)
    }

    /// Persist to an explicit path, creating parent directories.
    ///
    /// # Errors
    /// I/O errors creating the directory or writing the file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }

    /// Append `registry` to the search list if not already present. Returns
    /// `true` if it was added (idempotent: a duplicate returns `false`).
    pub fn add(&mut self, registry: &str) -> bool {
        let registry = registry.trim().trim_end_matches('/');
        if registry.is_empty() || self.search.iter().any(|r| r == registry) {
            return false;
        }
        self.search.push(registry.to_string());
        true
    }

    /// Remove `registry` from the search list. Returns `true` if it was present.
    pub fn remove(&mut self, registry: &str) -> bool {
        let registry = registry.trim().trim_end_matches('/');
        let before = self.search.len();
        self.search.retain(|r| r != registry);
        self.search.len() != before
    }
}

/// Whether `reference` already names a registry host (is "qualified"). The
/// segment before the first `/` is a host iff it contains `.` or `:`, or is
/// exactly `localhost` — the Docker / Podman rule. A single-segment name
/// (`alpine`, `alpine:3.23`) or a bare repo path (`library/alpine`) is
/// unqualified.
#[must_use]
pub fn is_qualified(reference: &str) -> bool {
    match reference.split_once('/') {
        Some((host, _)) => host.contains('.') || host.contains(':') || host == "localhost",
        None => false,
    }
}

/// Candidate references to try, in order, for `reference` given the operator's
/// `search` list.
///
/// A qualified reference (or an empty search list) yields just itself, so
/// behaviour is unchanged unless the operator configured registries. An
/// unqualified reference yields `<each search registry>/<reference>` in order,
/// then `reference` itself as the final fallback (which the OCI reference
/// parser defaults to Docker Hub).
#[must_use]
pub fn resolution_candidates(reference: &str, search: &[String]) -> Vec<String> {
    if search.is_empty() || is_qualified(reference) {
        return vec![reference.to_string()];
    }
    let mut out: Vec<String> = search
        .iter()
        .map(|reg| format!("{}/{reference}", reg.trim_end_matches('/')))
        .collect();
    out.push(reference.to_string());
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn is_qualified_follows_the_docker_host_rule() {
        // Hosts: a dot, a port colon, or exactly `localhost`.
        assert!(is_qualified("registry.example.com/app:1"));
        assert!(is_qualified("ghcr.io/owner/app"));
        assert!(is_qualified("localhost:5000/app"));
        assert!(is_qualified("localhost/app"));
        // Unqualified: a single name, a tagged single name, or a bare repo path.
        assert!(!is_qualified("alpine"));
        assert!(!is_qualified("alpine:3.23"));
        assert!(!is_qualified("library/alpine"));
    }

    #[test]
    fn candidates_unchanged_without_search_registries() {
        // Empty search list: every reference resolves to itself, exactly as
        // before this feature.
        assert_eq!(
            resolution_candidates("alpine:3.23", &[]),
            vec!["alpine:3.23".to_string()],
        );
    }

    #[test]
    fn candidates_prepend_search_registries_then_docker_for_unqualified() {
        let search = vec!["registry.example.com".to_string(), "ghcr.io".to_string()];
        assert_eq!(
            resolution_candidates("alpine:3.23", &search),
            vec![
                "registry.example.com/alpine:3.23".to_string(),
                "ghcr.io/alpine:3.23".to_string(),
                "alpine:3.23".to_string(),
            ],
        );
    }

    #[test]
    fn candidates_leave_qualified_references_untouched() {
        let search = vec!["registry.example.com".to_string()];
        // A fully-qualified reference is never rewritten, even with search
        // registries configured.
        assert_eq!(
            resolution_candidates("ghcr.io/owner/app:2", &search),
            vec!["ghcr.io/owner/app:2".to_string()],
        );
    }

    #[test]
    fn add_is_idempotent_and_trims() {
        let mut cfg = SearchRegistries::default();
        assert!(cfg.add("registry.example.com"));
        assert!(!cfg.add("registry.example.com"), "duplicate not re-added");
        assert!(!cfg.add("  registry.example.com/ "), "trimmed duplicate");
        assert!(cfg.add("ghcr.io"));
        assert_eq!(cfg.search, vec!["registry.example.com", "ghcr.io"]);
    }

    #[test]
    fn remove_reports_presence() {
        let mut cfg = SearchRegistries {
            search: vec!["a.io".to_string(), "b.io".to_string()],
        };
        assert!(cfg.remove("a.io"));
        assert!(!cfg.remove("a.io"), "already gone");
        assert_eq!(cfg.search, vec!["b.io"]);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("umf").join("registries.toml");
        let cfg = SearchRegistries {
            search: vec!["registry.example.com".to_string(), "ghcr.io".to_string()],
        };
        cfg.save_to(&path).expect("save");
        let loaded = SearchRegistries::load_from(&path);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn load_from_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = SearchRegistries::load_from(&dir.path().join("nope.toml"));
        assert!(loaded.search.is_empty());
    }
}
