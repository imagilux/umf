//! OCI label key and value newtypes.

use std::fmt;

use thiserror::Error;

use crate::types::ValidationError;

// ════════════════════════════════════════════════════════════════════════════
// LabelKey — OCI label key (reverse-DNS, lowercase alnum + `.` + `-`)
// ════════════════════════════════════════════════════════════════════════════

/// An OCI label key.
///
/// Grammar (per the OCI image spec annotation key conventions): reverse-DNS;
/// lowercase ASCII alphanumeric + `.` + `-`; each `.`-separated segment
/// non-empty and starts with a letter; no consecutive separators; no leading
/// or trailing separator.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LabelKey(String);

impl LabelKey {
    /// Parse and validate a label key.
    ///
    /// # Errors
    /// Returns a [`LabelKeyError`] when the input violates the OCI label key
    /// grammar.
    pub fn new(raw: impl Into<String>) -> Result<Self, LabelKeyError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(LabelKeyError::Empty);
        }
        let bytes = raw.as_bytes();
        // First char of the whole key must start a segment — letter.
        if !bytes[0].is_ascii_lowercase() {
            return Err(LabelKeyError::InvalidSegmentStart {
                ch: raw.chars().next().unwrap_or('?'),
                offset: 0,
            });
        }
        let mut prev_was_separator = false;
        for (i, ch) in raw.char_indices() {
            match ch {
                'a'..='z' | '0'..='9' => prev_was_separator = false,
                '.' | '-' => {
                    if prev_was_separator {
                        return Err(LabelKeyError::ConsecutiveSeparators { offset: i });
                    }
                    if i == 0 {
                        return Err(LabelKeyError::LeadingSeparator { ch, offset: 0 });
                    }
                    prev_was_separator = true;
                }
                _ => return Err(LabelKeyError::InvalidChar { ch, offset: i }),
            }
        }
        if prev_was_separator {
            return Err(LabelKeyError::TrailingSeparator {
                offset: raw.len().saturating_sub(1),
            });
        }
        // Reverse-DNS shape: each `.`-segment must start with a letter.
        let mut segment_start = 0;
        for (i, ch) in raw.char_indices() {
            if ch == '.' {
                let segment = &raw[segment_start..i];
                if let Some(first) = segment.chars().next()
                    && !first.is_ascii_lowercase()
                {
                    return Err(LabelKeyError::InvalidSegmentStart {
                        ch: first,
                        offset: segment_start,
                    });
                }
                segment_start = i + 1;
            }
        }
        let last_segment = &raw[segment_start..];
        if let Some(first) = last_segment.chars().next()
            && !first.is_ascii_lowercase()
        {
            return Err(LabelKeyError::InvalidSegmentStart {
                ch: first,
                offset: segment_start,
            });
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for LabelKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LabelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`LabelKey`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LabelKeyError {
    /// The input string is empty.
    #[error("label key cannot be empty")]
    Empty,
    /// A `.`-separated segment starts with something other than a lowercase letter.
    #[error("label key segment must start with a lowercase letter, not `{ch}`")]
    InvalidSegmentStart {
        /// The offending character.
        ch: char,
        /// Byte offset where the segment starts.
        offset: usize,
    },
    /// A character outside `[a-z0-9.-]` appeared.
    #[error("label key may only contain lowercase letters, digits, `.`, and `-` — found `{ch}`")]
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
    /// `.` or `-` appeared at the start.
    #[error("label key cannot start with `{ch}`")]
    LeadingSeparator {
        /// The leading separator character.
        ch: char,
        /// Always 0; kept for uniform mapping into the parser sub-span helper.
        offset: usize,
    },
    /// `.` or `-` appeared at the end.
    #[error("label key cannot end with a separator")]
    TrailingSeparator {
        /// Byte offset of the trailing separator.
        offset: usize,
    },
    /// Two separators (`.` or `-`) appeared back-to-back.
    #[error("label key cannot contain consecutive separators")]
    ConsecutiveSeparators {
        /// Byte offset of the second separator.
        offset: usize,
    },
}

impl ValidationError for LabelKeyError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::InvalidSegmentStart { offset, .. }
            | Self::InvalidChar { offset, .. }
            | Self::LeadingSeparator { offset, .. }
            | Self::TrailingSeparator { offset }
            | Self::ConsecutiveSeparators { offset } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("OCI label keys are reverse-DNS, e.g. `org.imagilux.umf.author`")
    }
}

// ════════════════════════════════════════════════════════════════════════════
// LabelValue — UTF-8, no NUL byte
// ════════════════════════════════════════════════════════════════════════════

/// An OCI label value.
///
/// Labels are otherwise opaque per the OCI image spec; we only forbid embedded
/// NUL bytes, which would break downstream OCI manifest emission.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LabelValue(String);

impl LabelValue {
    /// Validate a label value.
    ///
    /// # Errors
    /// Returns a [`LabelValueError`] when the input contains a NUL byte.
    pub fn new(raw: impl Into<String>) -> Result<Self, LabelValueError> {
        let raw = raw.into();
        if let Err(offset) = super::reject_nul(&raw) {
            return Err(LabelValueError::NulByte { offset });
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for LabelValue {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LabelValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`LabelValue`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LabelValueError {
    /// The value contains a NUL byte.
    #[error("label value cannot contain a NUL byte")]
    NulByte {
        /// Byte offset of the NUL.
        offset: usize,
    },
}

impl ValidationError for LabelValueError {
    fn offset(&self) -> Option<usize> {
        let Self::NulByte { offset } = self;
        Some(*offset)
    }
}

#[cfg(test)]
mod tests;
