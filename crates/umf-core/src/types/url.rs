//! URL-class newtypes — currently a permissive `HttpsUrl` used as the
//! `Url` variant of [`crate::ast::AddSource`].
//!
//! The validator here is intentionally minimal: it checks the scheme,
//! requires a non-empty authority, and forbids embedded NUL bytes. A full
//! URL parser is deliberately avoided so that `umf-core` stays dependency-light;
//! anything beyond what we catch is surfaced later by the builder's HTTP client.

use std::fmt;

use thiserror::Error;

use super::ValidationError;

/// An HTTP / HTTPS URL used by `ADD <url> <dst>`.
///
/// Validation:
/// - Begins with `http://` or `https://`.
/// - Has at least one non-empty character after the scheme.
/// - No NUL byte.
///
/// Full URL grammar (percent encoding, IPv6 brackets, query strings) is
/// not enforced — the HTTP client doing the fetch is the source of truth.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpsUrl {
    raw: String,
    /// Byte offset of the start of the authority (after `://`).
    authority_start: usize,
    /// True if the scheme is `https`.
    https: bool,
}

impl HttpsUrl {
    /// Parse and validate an HTTP/HTTPS URL.
    ///
    /// # Errors
    /// Returns an [`HttpsUrlError`] when the input is missing the scheme,
    /// has an empty authority, or contains a NUL byte.
    pub fn new(raw: impl Into<String>) -> Result<Self, HttpsUrlError> {
        let raw = raw.into();
        let (authority_start, https) = if let Some(rest) = raw.strip_prefix("https://") {
            if rest.is_empty() {
                return Err(HttpsUrlError::EmptyAuthority);
            }
            (8, true)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            if rest.is_empty() {
                return Err(HttpsUrlError::EmptyAuthority);
            }
            (7, false)
        } else {
            return Err(HttpsUrlError::MissingScheme);
        };
        if let Some(pos) = raw.find('\0') {
            return Err(HttpsUrlError::NulByte { offset: pos });
        }
        Ok(Self {
            raw,
            authority_start,
            https,
        })
    }

    /// Parse an HTTP/HTTPS URL that may carry build-time `${VAR}` / `$VAR`
    /// placeholders.
    ///
    /// A URL with no placeholder is validated by [`new`](Self::new). One that
    /// *does* contain a placeholder still has its scheme checked (a placeholder
    /// can't change `http://` / `https://`, and the parser only reaches this
    /// path having matched the literal scheme) and its NUL-freeness enforced,
    /// but the authority / path are left for the builder to validate after it
    /// substitutes the `ARG` scope. Mirrors
    /// [`OciReference::new_allowing_placeholders`](crate::types::OciReference::new_allowing_placeholders)
    /// so `ADD https://host/${VER}.tar /` parses and resolves at build time.
    ///
    /// # Errors
    /// Returns an [`HttpsUrlError`] for a missing scheme, an empty authority, or
    /// a NUL byte — and, when no placeholder is present, any other strict-grammar
    /// violation [`new`](Self::new) catches.
    pub fn new_allowing_placeholders(raw: impl Into<String>) -> Result<Self, HttpsUrlError> {
        let raw = raw.into();
        if !crate::subst::contains_placeholder(&raw) {
            return Self::new(raw);
        }
        let (authority_start, https) = if let Some(rest) = raw.strip_prefix("https://") {
            if rest.is_empty() {
                return Err(HttpsUrlError::EmptyAuthority);
            }
            (8, true)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            if rest.is_empty() {
                return Err(HttpsUrlError::EmptyAuthority);
            }
            (7, false)
        } else {
            return Err(HttpsUrlError::MissingScheme);
        };
        if let Some(pos) = raw.find('\0') {
            return Err(HttpsUrlError::NulByte { offset: pos });
        }
        Ok(Self {
            raw,
            authority_start,
            https,
        })
    }

    /// Borrow the underlying URL string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The substring after the scheme (everything past `://`).
    #[must_use]
    pub fn authority_and_path(&self) -> &str {
        &self.raw[self.authority_start..]
    }

    /// `true` when the scheme is `https://`.
    #[must_use]
    pub const fn is_https(&self) -> bool {
        self.https
    }
}

impl AsRef<str> for HttpsUrl {
    fn as_ref(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for HttpsUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Why an [`HttpsUrl`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HttpsUrlError {
    /// The input does not begin with `http://` or `https://`.
    #[error("URL must begin with `http://` or `https://`")]
    MissingScheme,
    /// The scheme is present but the authority is empty.
    #[error("URL has no authority after the scheme")]
    EmptyAuthority,
    /// The URL contains a NUL byte.
    #[error("URL cannot contain a NUL byte")]
    NulByte {
        /// Byte offset of the NUL.
        offset: usize,
    },
}

impl ValidationError for HttpsUrlError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::MissingScheme => Some(0),
            Self::EmptyAuthority => None,
            Self::NulByte { offset } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("only `http://` and `https://` are recognised — local files use a plain path")
    }
}

#[cfg(test)]
mod tests;
