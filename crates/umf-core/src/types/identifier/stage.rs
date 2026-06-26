//! Short build-time identifier newtypes — stage names and secret ids.
//!
//! Both share the bare-identifier grammar `[A-Za-z_][A-Za-z0-9_-]*`.

use std::fmt;

use thiserror::Error;

use crate::types::ValidationError;

/// A bare-identifier grammar violation, before it is mapped onto a specific
/// newtype's error enum.
///
/// [`StageName`] and [`SecretId`] share the exact grammar
/// `[A-Za-z_][A-Za-z0-9_-]*`; this lets both validate through one routine while
/// keeping their distinct public error types and messages.
enum BareIdentifierViolation {
    Empty,
    InvalidLeadingChar { ch: char },
    InvalidChar { ch: char, offset: usize },
}

/// Validate a bare identifier (`[A-Za-z_][A-Za-z0-9_-]*`).
fn validate_bare_identifier(raw: &str) -> Result<(), BareIdentifierViolation> {
    let mut chars = raw.char_indices();
    let Some((_, first)) = chars.next() else {
        return Err(BareIdentifierViolation::Empty);
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(BareIdentifierViolation::InvalidLeadingChar { ch: first });
    }
    for (i, ch) in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            return Err(BareIdentifierViolation::InvalidChar { ch, offset: i });
        }
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════════
// StageName — bare identifier (`[A-Za-z_][A-Za-z0-9_-]*`)
// ════════════════════════════════════════════════════════════════════════════

/// A multi-stage stage name (`FROM ... AS <name>` and `ADD --from=<name>`).
///
/// Grammar: `[A-Za-z_][A-Za-z0-9_-]*` — bare identifier shape, dash allowed
/// after the first character.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StageName(String);

impl StageName {
    /// Parse and validate a stage name.
    ///
    /// # Errors
    /// Returns a [`StageNameError`] when the input is empty, starts with a
    /// digit or dash, or contains an invalid character.
    pub fn new(raw: impl Into<String>) -> Result<Self, StageNameError> {
        let raw = raw.into();
        match validate_bare_identifier(&raw) {
            Ok(()) => Ok(Self(raw)),
            Err(BareIdentifierViolation::Empty) => Err(StageNameError::Empty),
            Err(BareIdentifierViolation::InvalidLeadingChar { ch }) => {
                Err(StageNameError::InvalidLeadingChar { ch })
            }
            Err(BareIdentifierViolation::InvalidChar { ch, offset }) => {
                Err(StageNameError::InvalidChar { ch, offset })
            }
        }
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for StageName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`StageName`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StageNameError {
    /// The input is empty.
    #[error("stage name cannot be empty")]
    Empty,
    /// The first character is not a letter or `_`.
    #[error("stage name must start with a letter or `_`, not `{ch}`")]
    InvalidLeadingChar {
        /// The offending leading character.
        ch: char,
    },
    /// A subsequent character is outside `[A-Za-z0-9_-]`.
    #[error("stage name may only contain letters, digits, `_`, and `-` — found `{ch}`")]
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
}

impl ValidationError for StageNameError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::InvalidLeadingChar { .. } => Some(0),
            Self::InvalidChar { offset, .. } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("stage names look like identifiers: e.g. `builder`, `runtime-stage`")
    }
}

// ════════════════════════════════════════════════════════════════════════════
// SecretId — `[A-Za-z_][A-Za-z0-9_-]*`
// ════════════════════════════════════════════════════════════════════════════

/// A build secret id (`RUN --mount=type=secret,id=<id>`).
///
/// Same grammar as a stage name — bare identifier with `-` allowed after the
/// first character. Matches the BuildKit convention.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretId(String);

impl SecretId {
    /// Parse and validate a secret id.
    ///
    /// # Errors
    /// Returns a [`SecretIdError`] when the input is empty, starts with a
    /// digit or dash, or contains an invalid character.
    pub fn new(raw: impl Into<String>) -> Result<Self, SecretIdError> {
        let raw = raw.into();
        match validate_bare_identifier(&raw) {
            Ok(()) => Ok(Self(raw)),
            Err(BareIdentifierViolation::Empty) => Err(SecretIdError::Empty),
            Err(BareIdentifierViolation::InvalidLeadingChar { ch }) => {
                Err(SecretIdError::InvalidLeadingChar { ch })
            }
            Err(BareIdentifierViolation::InvalidChar { ch, offset }) => {
                Err(SecretIdError::InvalidChar { ch, offset })
            }
        }
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SecretId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`SecretId`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SecretIdError {
    /// The input is empty.
    #[error("secret id cannot be empty")]
    Empty,
    /// The first character is not a letter or `_`.
    #[error("secret id must start with a letter or `_`, not `{ch}`")]
    InvalidLeadingChar {
        /// The offending leading character.
        ch: char,
    },
    /// A subsequent character is outside `[A-Za-z0-9_-]`.
    #[error("secret id may only contain letters, digits, `_`, and `-` — found `{ch}`")]
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
}

impl ValidationError for SecretIdError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::InvalidLeadingChar { .. } => Some(0),
            Self::InvalidChar { offset, .. } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("secret ids look like identifiers: e.g. `signing-key`")
    }
}

#[cfg(test)]
mod tests;
