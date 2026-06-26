//! Environment / ARG name and value newtypes.

use std::fmt;

use thiserror::Error;

use crate::types::ValidationError;

// ════════════════════════════════════════════════════════════════════════════
// EnvVarName — POSIX env var name (`[A-Za-z_][A-Za-z0-9_]*`)
// ════════════════════════════════════════════════════════════════════════════

/// A POSIX environment variable name.
///
/// Grammar: `[A-Za-z_][A-Za-z0-9_]*` — the rule used by every POSIX shell.
/// Also used for `ARG` names, which carry the same constraints.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvVarName(String);

impl EnvVarName {
    /// Parse and validate an env var name.
    ///
    /// # Errors
    /// Returns an [`EnvVarNameError`] when the input is empty, starts with a
    /// digit, or contains a character outside `[A-Za-z0-9_]`.
    pub fn new(raw: impl Into<String>) -> Result<Self, EnvVarNameError> {
        let raw = raw.into();
        let mut chars = raw.char_indices();
        let Some((_, first)) = chars.next() else {
            return Err(EnvVarNameError::Empty);
        };
        if !(first.is_ascii_alphabetic() || first == '_') {
            return Err(EnvVarNameError::InvalidLeadingChar { ch: first });
        }
        for (i, ch) in chars {
            if !(ch.is_ascii_alphanumeric() || ch == '_') {
                return Err(EnvVarNameError::InvalidChar { ch, offset: i });
            }
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EnvVarName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvVarName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an [`EnvVarName`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvVarNameError {
    /// The input is empty.
    #[error("environment variable name cannot be empty")]
    Empty,
    /// The first character is not a letter or `_`.
    #[error("environment variable name must start with a letter or `_`, not `{ch}`")]
    InvalidLeadingChar {
        /// The offending leading character.
        ch: char,
    },
    /// A subsequent character is not `[A-Za-z0-9_]`.
    #[error("environment variable name may only contain letters, digits, and `_` — found `{ch}`")]
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
}

impl ValidationError for EnvVarNameError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::InvalidLeadingChar { .. } => Some(0),
            Self::InvalidChar { offset, .. } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("env var names follow POSIX: `[A-Za-z_][A-Za-z0-9_]*`")
    }
}

// ════════════════════════════════════════════════════════════════════════════
// EnvVarValue — UTF-8, no NUL byte
// ════════════════════════════════════════════════════════════════════════════

/// An environment variable value (or ARG default).
///
/// We only forbid embedded NUL bytes, which the kernel uses as the argv
/// separator and which break OCI manifest serialization.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvVarValue(String);

impl EnvVarValue {
    /// Validate an env var value.
    ///
    /// # Errors
    /// Returns an [`EnvVarValueError`] when the input contains a NUL byte.
    pub fn new(raw: impl Into<String>) -> Result<Self, EnvVarValueError> {
        let raw = raw.into();
        if let Err(offset) = super::reject_nul(&raw) {
            return Err(EnvVarValueError::NulByte { offset });
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EnvVarValue {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvVarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an [`EnvVarValue`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvVarValueError {
    /// The value contains a NUL byte.
    #[error("environment variable value cannot contain a NUL byte")]
    NulByte {
        /// Byte offset of the NUL.
        offset: usize,
    },
}

impl ValidationError for EnvVarValueError {
    fn offset(&self) -> Option<usize> {
        let Self::NulByte { offset } = self;
        Some(*offset)
    }
}

#[cfg(test)]
mod tests;
