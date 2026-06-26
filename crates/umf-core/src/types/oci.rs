//! OCI distribution-spec reference newtype.
//!
//! Implements the reference grammar from the OCI Distribution Specification:
//!
//! ```text
//! reference      := name [":" tag] ["@" digest]
//! name           := [domain "/"] path-component ["/" path-component]*
//! domain         := host [":" port]
//! host           := domain-name | IPv4address | "[" IPv6address "]"
//! domain-name    := domain-component ["." domain-component]*
//! domain-component := /([A-Za-z0-9]|[A-Za-z0-9][A-Za-z0-9-]*[A-Za-z0-9])/
//! path-component := alpha-numeric [separator alpha-numeric]*
//! alpha-numeric  := /[a-z0-9]+/
//! separator      := /[_.]|__|[-]*/
//! tag            := /[\w][\w.-]{0,127}/
//! digest         := algorithm ":" hex
//! ```
//!
//! Host vs. first-path-component disambiguation follows the established
//! convention: the first slash-separated segment is treated as a host iff
//! it contains a `.`, contains a `:`, or is literally `localhost`.
//! Otherwise the whole pre-tag string is the path and the caller's default
//! registry resolves the host at fetch time.
//!
//! This is a validated newtype: [`OciReference::new`] runs the full grammar to
//! reject malformed input, but only the raw string is kept. Structural
//! components are re-derived downstream via `oci_client::Reference` at fetch
//! time, so they are not stored here.

use std::fmt;

use thiserror::Error;

use super::ValidationError;

/// An OCI distribution-spec reference — a validated newtype over the raw
/// reference text.
///
/// [`OciReference::new`] validates the full distribution-spec grammar; only the
/// raw string is kept. Structural components (`host`, `port`, `repository`,
/// `tag`, `digest`) are re-derived downstream via `oci_client::Reference` at
/// resolve time, so they aren't stored or exposed here.
#[cfg_attr(feature = "serde", derive(serde::Serialize), serde(transparent))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OciReference {
    raw: String,
}

impl OciReference {
    /// Parse and validate an OCI reference.
    ///
    /// # Errors
    /// Returns an [`OciReferenceError`] when the input violates the
    /// distribution-spec grammar.
    pub fn new(raw: impl Into<String>) -> Result<Self, OciReferenceError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(OciReferenceError::Empty);
        }
        if let Some(pos) = raw.find('\0') {
            return Err(OciReferenceError::NulByte { offset: pos });
        }

        // `OciReference` is a *validated newtype*: it keeps only the raw string.
        // We run the full distribution-spec parse purely to validate — the
        // decomposed host / port / repository / tag / digest are discarded,
        // since the builder re-derives them via `oci_client::Reference` when it
        // actually resolves the ref (one authoritative grammar at resolve time).

        // Peel off the optional `@<digest>` suffix first — `@` is unambiguous,
        // it cannot appear anywhere else in the grammar.
        let name_and_tag = if let Some(at_pos) = raw.find('@') {
            parse_digest(&raw[at_pos + 1..], at_pos + 1)?;
            &raw[..at_pos]
        } else {
            raw.as_str()
        };

        // Peel off the optional `:<tag>`. The tricky case: a host port also
        // uses `:`, so we look for the *last* `:` and check whether what's to
        // its left contains a `/` (then it's a tag) or doesn't (then it could
        // be a port — but only if there's no `/` at all in `name_and_tag`,
        // i.e. no host segment).
        let (name, _tag) = split_tag(name_and_tag, &raw)?;

        // Validate the host / port / repository structure.
        split_host(name, name.as_ptr() as usize - raw.as_ptr() as usize, &raw)?;

        Ok(Self { raw })
    }

    /// Parse an OCI reference that may carry build-time `${VAR}` / `$VAR`
    /// placeholders.
    ///
    /// A reference with no placeholder is validated in full by [`new`](Self::new).
    /// One that *does* contain a placeholder cannot satisfy the distribution-spec
    /// grammar until the placeholder is resolved, so only minimal safety checks
    /// run here (non-empty, no NUL); the builder substitutes and then re-validates
    /// the result with [`new`](Self::new) before resolving it. This keeps the
    /// strict grammar as the single authority while letting `FROM myapp:${TAG}`
    /// and `ADD repo:${TAG} /` parse.
    ///
    /// # Errors
    /// Returns an [`OciReferenceError`] for an empty or NUL-bearing input, or —
    /// when no placeholder is present — any strict-grammar violation.
    pub fn new_allowing_placeholders(raw: impl Into<String>) -> Result<Self, OciReferenceError> {
        let raw = raw.into();
        if !crate::subst::contains_placeholder(&raw) {
            return Self::new(raw);
        }
        if raw.is_empty() {
            return Err(OciReferenceError::Empty);
        }
        if let Some(pos) = raw.find('\0') {
            return Err(OciReferenceError::NulByte { offset: pos });
        }
        Ok(Self { raw })
    }

    /// The full reference string as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl AsRef<str> for OciReference {
    fn as_ref(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for OciReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

// ── parser internals ────────────────────────────────────────────────────────

fn parse_digest(body: &str, base_offset: usize) -> Result<(), OciReferenceError> {
    let Some(colon) = body.find(':') else {
        return Err(OciReferenceError::DigestMissingColon {
            offset: base_offset,
        });
    };
    let algorithm = &body[..colon];
    let hex = &body[colon + 1..];
    if algorithm.is_empty() {
        return Err(OciReferenceError::DigestMissingAlgorithm {
            offset: base_offset,
        });
    }
    if hex.is_empty() {
        return Err(OciReferenceError::DigestMissingHex {
            offset: base_offset + colon + 1,
        });
    }
    // algorithm-component := /[a-z0-9]+/, separator := /[+._-]/
    for (i, ch) in algorithm.char_indices() {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '+' | '.' | '_' | '-'))
        {
            return Err(OciReferenceError::DigestBadAlgorithmChar {
                ch,
                offset: base_offset + i,
            });
        }
    }
    // hex := /[0-9a-fA-F]{32,}/
    if hex.len() < 32 {
        return Err(OciReferenceError::DigestHexTooShort {
            len: hex.len(),
            offset: base_offset + colon + 1,
        });
    }
    for (i, ch) in hex.char_indices() {
        if !ch.is_ascii_hexdigit() {
            return Err(OciReferenceError::DigestBadHexChar {
                ch,
                offset: base_offset + colon + 1 + i,
            });
        }
    }
    Ok(())
}

/// Decide where the tag (if any) starts inside `name_and_tag` and validate it.
///
/// `name_and_tag` is always a prefix slice of `raw`, so byte offsets are
/// recovered from the slice's position within `raw` (the `as_ptr` subtraction
/// below) rather than threaded as a separate argument.
fn split_tag<'a>(
    name_and_tag: &'a str,
    raw: &str,
) -> Result<(&'a str, Option<String>), OciReferenceError> {
    // Find the last `:` that could be a tag separator.
    // Within name_and_tag: the only `:` that could be a port colon is one
    // that lies in the first slash-separated segment (i.e. has no `/` before
    // it that's also after a preceding `:`). Simpler rule: if the last `:`
    // is followed by anything that contains a `/`, it's not a tag.
    let Some(last_colon) = name_and_tag.rfind(':') else {
        return Ok((name_and_tag, None));
    };
    let after = &name_and_tag[last_colon + 1..];
    if after.contains('/') {
        // The last `:` is inside the host segment (port). No tag.
        return Ok((name_and_tag, None));
    }
    // Otherwise treat it as a tag — unless we have a host but no path: that
    // would actually be `host:port` with no repo, which is invalid anyway.
    let before = &name_and_tag[..last_colon];
    if before.is_empty() {
        return Err(OciReferenceError::EmptyName {
            offset: name_and_tag.as_ptr() as usize - raw.as_ptr() as usize,
        });
    }
    let tag = after;
    if tag.is_empty() {
        return Err(OciReferenceError::EmptyTag {
            offset: last_colon + 1,
        });
    }
    validate_tag(tag, last_colon + 1)?;
    Ok((before, Some(tag.to_string())))
}

fn validate_tag(tag: &str, base_offset: usize) -> Result<(), OciReferenceError> {
    if tag.len() > 128 {
        return Err(OciReferenceError::TagTooLong {
            len: tag.len(),
            offset: base_offset,
        });
    }
    let mut chars = tag.char_indices();
    let Some((_, first)) = chars.next() else {
        return Err(OciReferenceError::EmptyTag {
            offset: base_offset,
        });
    };
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return Err(OciReferenceError::TagBadLeadingChar {
            ch: first,
            offset: base_offset,
        });
    }
    for (i, ch) in chars {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')) {
            return Err(OciReferenceError::TagBadChar {
                ch,
                offset: base_offset + i,
            });
        }
    }
    Ok(())
}

/// Decide whether the first slash-separated segment of `name` is a host or
/// a path component. Returns `(host, port, repository)` where `host` is `None`
/// when the caller's default registry should be used.
fn split_host(
    name: &str,
    base_offset: usize,
    raw: &str,
) -> Result<(Option<String>, Option<u16>, String), OciReferenceError> {
    if name.is_empty() {
        return Err(OciReferenceError::EmptyName {
            offset: base_offset,
        });
    }
    let Some(slash) = name.find('/') else {
        // No slash — this is a single path component, no host.
        validate_path_component(name, base_offset, raw)?;
        return Ok((None, None, name.to_string()));
    };
    let head = &name[..slash];
    let tail = &name[slash + 1..];
    let looks_like_host = head == "localhost" || head.contains('.') || head.contains(':');
    if looks_like_host {
        let (host, port) = parse_host(head, base_offset)?;
        validate_repository(tail, base_offset + slash + 1, raw)?;
        Ok((Some(host), port, tail.to_string()))
    } else {
        validate_repository(name, base_offset, raw)?;
        Ok((None, None, name.to_string()))
    }
}

fn parse_host(head: &str, base_offset: usize) -> Result<(String, Option<u16>), OciReferenceError> {
    let (host_str, port) = if let Some(port_colon) = head.rfind(':') {
        let host_part = &head[..port_colon];
        let port_str = &head[port_colon + 1..];
        if port_str.is_empty() {
            return Err(OciReferenceError::HostPortEmpty {
                offset: base_offset + port_colon + 1,
            });
        }
        let port = port_str
            .parse::<u16>()
            .map_err(|_| OciReferenceError::HostPortInvalid {
                offset: base_offset + port_colon + 1,
                got: port_str.to_string(),
            })?;
        (host_part, Some(port))
    } else {
        (head, None)
    };
    validate_host(host_str, base_offset)?;
    Ok((host_str.to_string(), port))
}

fn validate_host(host: &str, base_offset: usize) -> Result<(), OciReferenceError> {
    if host.is_empty() {
        return Err(OciReferenceError::HostEmpty {
            offset: base_offset,
        });
    }
    // Each `.`-separated component matches /[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?/
    let mut start = 0;
    let bytes = host.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        if i == bytes.len() || bytes[i] == b'.' {
            let component = &host[start..i];
            if component.is_empty() {
                return Err(OciReferenceError::HostBadComponent {
                    offset: base_offset + start,
                });
            }
            // First char must be alnum, last must be alnum, middle may be alnum or `-`.
            let first = component.as_bytes()[0];
            let last = component.as_bytes()[component.len() - 1];
            if !is_ascii_alnum(first) || !is_ascii_alnum(last) {
                return Err(OciReferenceError::HostBadComponent {
                    offset: base_offset + start,
                });
            }
            for (j, ch) in component.char_indices() {
                if !(ch.is_ascii_alphanumeric() || ch == '-') {
                    return Err(OciReferenceError::HostBadChar {
                        ch,
                        offset: base_offset + start + j,
                    });
                }
            }
            start = i + 1;
        }
        i += 1;
    }
    Ok(())
}

const fn is_ascii_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn validate_repository(repo: &str, base_offset: usize, raw: &str) -> Result<(), OciReferenceError> {
    if repo.is_empty() {
        return Err(OciReferenceError::EmptyName {
            offset: base_offset,
        });
    }
    let mut start = 0;
    let bytes = repo.as_bytes();
    let mut i = 0;
    while i <= bytes.len() {
        if i == bytes.len() || bytes[i] == b'/' {
            let component = &repo[start..i];
            validate_path_component(component, base_offset + start, raw)?;
            start = i + 1;
        }
        i += 1;
    }
    Ok(())
}

fn validate_path_component(
    component: &str,
    base_offset: usize,
    _raw: &str,
) -> Result<(), OciReferenceError> {
    if component.is_empty() {
        return Err(OciReferenceError::PathComponentEmpty {
            offset: base_offset,
        });
    }
    // Grammar: alpha-numeric (separator alpha-numeric)*
    //   alpha-numeric := [a-z0-9]+
    //   separator     := [_.] | __ | [-]*  (one of those alternatives, between runs)
    // We enforce: only [a-z0-9._-] characters; the first and last char must be
    // alnum; no consecutive `.`; no `__` more than 2 long; no leading/trailing `-`.
    let bytes = component.as_bytes();
    let first = bytes[0];
    let last = bytes[component.len() - 1];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(OciReferenceError::PathComponentBadEdgeChar {
            ch: first as char,
            offset: base_offset,
        });
    }
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(OciReferenceError::PathComponentBadEdgeChar {
            ch: last as char,
            offset: base_offset + component.len() - 1,
        });
    }
    let mut prev_was_dot = false;
    let mut underscore_run = 0u32;
    for (i, ch) in component.char_indices() {
        match ch {
            'a'..='z' | '0'..='9' => {
                prev_was_dot = false;
                underscore_run = 0;
            }
            '.' => {
                if prev_was_dot {
                    return Err(OciReferenceError::PathComponentBadSeparator {
                        offset: base_offset + i,
                    });
                }
                prev_was_dot = true;
                underscore_run = 0;
            }
            '_' => {
                underscore_run += 1;
                if underscore_run > 2 {
                    return Err(OciReferenceError::PathComponentBadSeparator {
                        offset: base_offset + i,
                    });
                }
                prev_was_dot = false;
            }
            '-' => {
                prev_was_dot = false;
                underscore_run = 0;
            }
            _ => {
                return Err(OciReferenceError::PathComponentBadChar {
                    ch,
                    offset: base_offset + i,
                });
            }
        }
    }
    Ok(())
}

/// Why an [`OciReference`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OciReferenceError {
    /// The input is empty.
    #[error("OCI reference cannot be empty")]
    Empty,
    /// The input contains a NUL byte.
    #[error("OCI reference cannot contain a NUL byte")]
    NulByte {
        /// Byte offset of the NUL.
        offset: usize,
    },
    /// The repository portion (everything between the host and the tag) is empty.
    #[error("OCI reference is missing the repository name")]
    EmptyName {
        /// Byte offset where the (missing) name was expected.
        offset: usize,
    },
    /// The tag is present (with `:`) but empty.
    #[error("OCI reference tag cannot be empty")]
    EmptyTag {
        /// Byte offset of the start of the (empty) tag.
        offset: usize,
    },
    /// The tag exceeds 128 characters.
    #[error("OCI reference tag is {len} chars — distribution-spec limit is 128")]
    TagTooLong {
        /// Actual length.
        len: usize,
        /// Byte offset of the start of the tag.
        offset: usize,
    },
    /// The tag's first character is not alphanumeric or `_`.
    #[error("OCI reference tag must start with an alphanumeric character or `_`, found `{ch}`")]
    TagBadLeadingChar {
        /// Offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
    /// The tag contains a disallowed character.
    #[error("OCI reference tag contains invalid character `{ch}`")]
    TagBadChar {
        /// Offending character.
        ch: char,
        /// Byte offset.
        offset: usize,
    },
    /// The host is empty (before the `:port` or before the first `/`).
    #[error("OCI reference host cannot be empty")]
    HostEmpty {
        /// Byte offset where the host was expected.
        offset: usize,
    },
    /// A host component is empty (e.g. two dots in a row).
    #[error("OCI reference host has an empty component")]
    HostBadComponent {
        /// Byte offset of the bad component.
        offset: usize,
    },
    /// A host character is outside `[A-Za-z0-9.-]`.
    #[error("OCI reference host contains invalid character `{ch}`")]
    HostBadChar {
        /// Offending character.
        ch: char,
        /// Byte offset.
        offset: usize,
    },
    /// The host has a `:` but no port number after it.
    #[error("OCI reference host port cannot be empty")]
    HostPortEmpty {
        /// Byte offset where the port was expected.
        offset: usize,
    },
    /// The port number is not a valid `u16`.
    #[error("OCI reference host port `{got}` is not a valid 0..=65535 integer")]
    HostPortInvalid {
        /// Byte offset of the bad port.
        offset: usize,
        /// The string we tried to parse.
        got: String,
    },
    /// A path component (e.g. `imagilux` in `imagilux/kernel-linux`) is empty.
    #[error("OCI reference path component cannot be empty")]
    PathComponentEmpty {
        /// Byte offset of the empty component.
        offset: usize,
    },
    /// A path component starts or ends with a separator (not alnum).
    #[error("OCI reference path component edge must be `[a-z0-9]`, found `{ch}`")]
    PathComponentBadEdgeChar {
        /// Offending character.
        ch: char,
        /// Byte offset of the character.
        offset: usize,
    },
    /// A path component contains a disallowed character.
    #[error("OCI reference path component contains invalid character `{ch}`")]
    PathComponentBadChar {
        /// Offending character.
        ch: char,
        /// Byte offset.
        offset: usize,
    },
    /// A path component contains a disallowed separator run
    /// (e.g. `..`, `___`).
    #[error("OCI reference path component has a malformed separator run")]
    PathComponentBadSeparator {
        /// Byte offset of the bad separator.
        offset: usize,
    },
    /// The `@<digest>` suffix has no `:` between algorithm and hex.
    #[error("OCI reference digest must be `<algorithm>:<hex>`")]
    DigestMissingColon {
        /// Byte offset of the start of the digest.
        offset: usize,
    },
    /// The digest's algorithm part is empty.
    #[error("OCI reference digest is missing the algorithm")]
    DigestMissingAlgorithm {
        /// Byte offset of the start of the digest.
        offset: usize,
    },
    /// The digest's hex part is empty.
    #[error("OCI reference digest is missing the hex body")]
    DigestMissingHex {
        /// Byte offset where the hex was expected.
        offset: usize,
    },
    /// The digest hex body is shorter than 32 chars (the OCI distribution-spec floor).
    #[error("OCI reference digest hex body is {len} chars — needs at least 32")]
    DigestHexTooShort {
        /// Actual length.
        len: usize,
        /// Byte offset of the start of the hex.
        offset: usize,
    },
    /// The digest's algorithm contains a disallowed character.
    #[error("OCI reference digest algorithm contains invalid character `{ch}`")]
    DigestBadAlgorithmChar {
        /// Offending character.
        ch: char,
        /// Byte offset.
        offset: usize,
    },
    /// The digest's hex body contains a non-hex character.
    #[error("OCI reference digest hex body contains non-hex character `{ch}`")]
    DigestBadHexChar {
        /// Offending character.
        ch: char,
        /// Byte offset.
        offset: usize,
    },
}

impl ValidationError for OciReferenceError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::NulByte { offset }
            | Self::EmptyName { offset }
            | Self::EmptyTag { offset }
            | Self::TagTooLong { offset, .. }
            | Self::TagBadLeadingChar { offset, .. }
            | Self::TagBadChar { offset, .. }
            | Self::HostEmpty { offset }
            | Self::HostBadComponent { offset }
            | Self::HostBadChar { offset, .. }
            | Self::HostPortEmpty { offset }
            | Self::HostPortInvalid { offset, .. }
            | Self::PathComponentEmpty { offset }
            | Self::PathComponentBadEdgeChar { offset, .. }
            | Self::PathComponentBadChar { offset, .. }
            | Self::PathComponentBadSeparator { offset }
            | Self::DigestMissingColon { offset }
            | Self::DigestMissingAlgorithm { offset }
            | Self::DigestMissingHex { offset }
            | Self::DigestHexTooShort { offset, .. }
            | Self::DigestBadAlgorithmChar { offset, .. }
            | Self::DigestBadHexChar { offset, .. } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("examples: `alpine:3.21`, `imagilux/kernel-linux:7.0`, `quay.io/proj/img:tag`")
    }
}

#[cfg(test)]
mod tests;
