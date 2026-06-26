//! User-spec newtype for `USER <name>` (login name, numeric uid, or pair).

use std::fmt;

use thiserror::Error;

use crate::types::ValidationError;

// ════════════════════════════════════════════════════════════════════════════
// Username — POSIX login name (or numeric uid), with optional `:<group>`
// ════════════════════════════════════════════════════════════════════════════

/// A user spec for `USER <name>`.
///
/// Accepts a POSIX login name (`[a-z_][a-z0-9_-]*\\$?`, max 32 chars), a
/// numeric uid (`u32`), or a `user:group` pair using the same grammar for
/// each side. The newtype validates the syntax and stores the raw string;
/// downstream consumers reuse the string verbatim.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Username(String);

impl Username {
    /// Maximum length of a single login-name component, per POSIX `useradd`.
    pub const MAX_LOGIN_LEN: usize = 32;

    /// Parse and validate a user spec.
    ///
    /// # Errors
    /// Returns a [`UsernameError`] when the input is empty, has an empty
    /// user/group component around the `:`, exceeds the per-component length
    /// cap, or contains an invalid character.
    pub fn new(raw: impl Into<String>) -> Result<Self, UsernameError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(UsernameError::Empty);
        }
        let (user, group_offset) = match raw.find(':') {
            Some(pos) => (&raw[..pos], Some(pos + 1)),
            None => (raw.as_str(), None),
        };
        if user.is_empty() {
            return Err(UsernameError::EmptyUser);
        }
        validate_user_component(user, 0)?;
        if let Some(off) = group_offset {
            let group = &raw[off..];
            if group.is_empty() {
                return Err(UsernameError::EmptyGroup { offset: off });
            }
            validate_user_component(group, off)?;
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_user_component(component: &str, base_offset: usize) -> Result<(), UsernameError> {
    // Numeric uid/gid: must fit in u32.
    if component.chars().all(|c| c.is_ascii_digit()) {
        if component.parse::<u32>().is_err() {
            return Err(UsernameError::NumericOverflow {
                offset: base_offset,
            });
        }
        return Ok(());
    }
    if component.len() > Username::MAX_LOGIN_LEN {
        return Err(UsernameError::TooLong {
            offset: base_offset,
            len: component.len(),
        });
    }
    let mut chars = component.char_indices();
    // Safe to unwrap: caller guarantees `component` is non-empty.
    let Some((_, first)) = chars.next() else {
        return Err(UsernameError::Empty);
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return Err(UsernameError::InvalidLeadingChar {
            ch: first,
            offset: base_offset,
        });
    }
    let end_byte = component.len();
    let trailing_dollar = component.ends_with('$');
    let body_end = if trailing_dollar {
        end_byte - 1
    } else {
        end_byte
    };
    for (i, ch) in chars {
        if i >= body_end {
            break;
        }
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-') {
            return Err(UsernameError::InvalidChar {
                ch,
                offset: base_offset + i,
            });
        }
    }
    Ok(())
}

impl AsRef<str> for Username {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Username {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`Username`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UsernameError {
    /// The whole input is empty.
    #[error("user spec cannot be empty")]
    Empty,
    /// The part before `:` is empty.
    #[error("user component cannot be empty")]
    EmptyUser,
    /// The part after `:` is empty.
    #[error("group component cannot be empty")]
    EmptyGroup {
        /// Byte offset of the start of the (empty) group component.
        offset: usize,
    },
    /// A login-name component exceeds [`Username::MAX_LOGIN_LEN`] (32 chars).
    #[error("login-name component is {len} chars — POSIX limit is 32")]
    TooLong {
        /// Byte offset of the start of the over-long component.
        offset: usize,
        /// Actual length in bytes.
        len: usize,
    },
    /// A login-name component starts with something other than a lowercase
    /// letter or `_`.
    #[error("login name must start with a lowercase letter or `_`, not `{ch}`")]
    InvalidLeadingChar {
        /// The offending leading character.
        ch: char,
        /// Byte offset of the character (start of the component).
        offset: usize,
    },
    /// A login-name component contains a disallowed character.
    #[error("login name may only contain lowercase letters, digits, `_`, and `-` — found `{ch}`")]
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
    /// A numeric uid/gid component does not fit in `u32`.
    #[error("numeric uid/gid must fit in 32 bits")]
    NumericOverflow {
        /// Byte offset where the numeric component starts.
        offset: usize,
    },
}

impl ValidationError for UsernameError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty | Self::EmptyUser => None,
            Self::EmptyGroup { offset }
            | Self::TooLong { offset, .. }
            | Self::InvalidLeadingChar { offset, .. }
            | Self::InvalidChar { offset, .. }
            | Self::NumericOverflow { offset } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("examples: `nginx`, `1000`, `nginx:nginx`, `0:0`")
    }
}

#[cfg(test)]
mod tests;
