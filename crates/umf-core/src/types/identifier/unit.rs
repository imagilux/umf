//! systemd / OpenRC service unit name newtype and its unit-type suffix enum.

use std::fmt;

use thiserror::Error;

use crate::types::ValidationError;

// ════════════════════════════════════════════════════════════════════════════
// ServiceUnitName — systemd / OpenRC unit name with optional unit-type suffix
// ════════════════════════════════════════════════════════════════════════════

/// Known systemd unit-type suffixes (per `man systemd.unit`).
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitSuffix {
    /// `.service`
    Service,
    /// `.socket`
    Socket,
    /// `.timer`
    Timer,
    /// `.target`
    Target,
    /// `.path`
    Path,
    /// `.mount`
    Mount,
    /// `.device`
    Device,
    /// `.swap`
    Swap,
    /// `.slice`
    Slice,
    /// `.scope`
    Scope,
    /// `.automount`
    Automount,
}

impl UnitSuffix {
    /// String form of the suffix, without the leading `.`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Socket => "socket",
            Self::Timer => "timer",
            Self::Target => "target",
            Self::Path => "path",
            Self::Mount => "mount",
            Self::Device => "device",
            Self::Swap => "swap",
            Self::Slice => "slice",
            Self::Scope => "scope",
            Self::Automount => "automount",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "service" => Self::Service,
            "socket" => Self::Socket,
            "timer" => Self::Timer,
            "target" => Self::Target,
            "path" => Self::Path,
            "mount" => Self::Mount,
            "device" => Self::Device,
            "swap" => Self::Swap,
            "slice" => Self::Slice,
            "scope" => Self::Scope,
            "automount" => Self::Automount,
            _ => return None,
        })
    }
}

impl fmt::Display for UnitSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A systemd / OpenRC service unit name (e.g. the `nftables.service` the
/// `EXPOSE` firewall enables).
///
/// Grammar: characters in the set `[A-Za-z0-9_:.@-]+`, non-empty. When the
/// name ends with `.<known-suffix>` (one of [`UnitSuffix`]'s variants) the
/// suffix is parsed out and accessible via [`ServiceUnitName::suffix`]; the
/// part before the final `.` is the bare name available via
/// [`ServiceUnitName::bare_name`].
///
/// The builder uses these accessors to emit per-init-system file paths
/// without re-parsing the string at runtime: systemd writes
/// `<bare>.service` when there's no explicit suffix; OpenRC strips any
/// suffix.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceUnitName {
    raw: String,
    /// Byte offset where the suffix starts (after the final `.`), if any.
    suffix_start: Option<usize>,
    suffix: Option<UnitSuffix>,
}

impl ServiceUnitName {
    /// Parse and validate a service unit name.
    ///
    /// # Errors
    /// Returns a [`ServiceUnitNameError`] when the input is empty, contains a
    /// disallowed character, or ends with an unknown unit-type suffix (i.e.
    /// something that looks like a suffix but isn't one of the supported
    /// systemd unit types).
    pub fn new(raw: impl Into<String>) -> Result<Self, ServiceUnitNameError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(ServiceUnitNameError::Empty);
        }
        for (i, ch) in raw.char_indices() {
            if !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-' | '@' | ':')) {
                return Err(ServiceUnitNameError::InvalidChar { ch, offset: i });
            }
        }
        // Suffix detection: split on the last `.`. If the right side matches
        // a known UnitSuffix, treat it as the suffix; if it looks like one
        // (alpha-only, non-empty) but isn't known, reject as an unknown
        // suffix to catch typos like `.serivce`.
        if let Some(dot_pos) = raw.rfind('.') {
            let tail = &raw[dot_pos + 1..];
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_alphabetic()) {
                if let Some(suf) = UnitSuffix::from_str(tail) {
                    if dot_pos == 0 {
                        return Err(ServiceUnitNameError::EmptyBareName);
                    }
                    return Ok(Self {
                        raw,
                        suffix_start: Some(dot_pos + 1),
                        suffix: Some(suf),
                    });
                }
                return Err(ServiceUnitNameError::UnknownSuffix {
                    suffix: tail.to_string(),
                    offset: dot_pos + 1,
                });
            }
        }
        Ok(Self {
            raw,
            suffix_start: None,
            suffix: None,
        })
    }

    /// Full unit name including any suffix (`nginx.service`, `network`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The unit-type suffix, if one is explicitly present.
    #[must_use]
    pub const fn suffix(&self) -> Option<UnitSuffix> {
        self.suffix
    }

    /// The bare name (without any unit-type suffix).
    ///
    /// `ServiceUnitName::new("nginx.service")?.bare_name()` returns `"nginx"`;
    /// `ServiceUnitName::new("network")?.bare_name()` returns `"network"`.
    #[must_use]
    pub fn bare_name(&self) -> &str {
        match self.suffix_start {
            Some(pos) => &self.raw[..pos - 1],
            None => &self.raw,
        }
    }

    /// Return the unit name fully qualified with `default` if no suffix is set,
    /// otherwise the original name. Allocates only when adding a suffix.
    #[must_use]
    pub fn with_default_suffix(&self, default: UnitSuffix) -> String {
        if self.suffix.is_some() {
            self.raw.clone()
        } else {
            format!("{}.{}", self.raw, default.as_str())
        }
    }
}

impl AsRef<str> for ServiceUnitName {
    fn as_ref(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for ServiceUnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Why a [`ServiceUnitName`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ServiceUnitNameError {
    /// The input is empty.
    #[error("service unit name cannot be empty")]
    Empty,
    /// `.<known-suffix>` with nothing before the dot (e.g. `.service`).
    #[error("service unit name has an empty base — needs a name before the `.<suffix>`")]
    EmptyBareName,
    /// A character outside the allowed alphabet appeared.
    #[error(
        "service unit name may only contain letters, digits, `_`, `.`, `-`, `@`, and `:` — found `{ch}`"
    )]
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
    /// The trailing `.<word>` does not match any known systemd unit type.
    #[error(
        "unknown service unit suffix `.{suffix}` — expected one of `service`, `socket`, `timer`, `target`, `path`, `mount`, `device`, `swap`, `slice`, `scope`, `automount`"
    )]
    UnknownSuffix {
        /// The string after the trailing `.`.
        suffix: String,
        /// Byte offset where the suffix starts.
        offset: usize,
    },
}

impl ValidationError for ServiceUnitNameError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty | Self::EmptyBareName => None,
            Self::InvalidChar { offset, .. } | Self::UnknownSuffix { offset, .. } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("examples: `nginx`, `nginx.service`, `getty@tty1.service`")
    }
}

#[cfg(test)]
mod tests;
