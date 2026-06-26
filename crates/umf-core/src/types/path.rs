//! Path-class newtypes — filesystem paths inside the image.
//!
//! Two newtypes live here:
//!
//! - [`AbsolutePath`] — strict, normalized absolute path. Used internally when
//!   the surrounding code wants a guaranteed-absolute path with no trailing or
//!   doubled slashes.
//! - [`RecipePath`] — looser recipe-flavored path as written by the recipe
//!   author. Used for the user-facing directives where authors expect to
//!   write paths freely (`WORKDIR`, `ADD`/`COPY` destinations, `RUN --mount
//!   target`): may be relative or absolute, may end with `/` (semantically
//!   meaningful for ADD/COPY destinations — forces directory semantics),
//!   and is preserved verbatim so the original spelling survives into the
//!   downstream emitter.
//!
//! Both are stored as `String` rather than [`std::path::PathBuf`] because the
//! AST is `Serialize` and `PathBuf` does not round-trip cleanly through JSON.
//! The newtypes validate syntax and expose [`AsRef<Path>`] for the rare
//! consumer that does want a `Path`.

use std::fmt;
use std::path::Path;

use thiserror::Error;

use super::ValidationError;

/// An absolute POSIX filesystem path inside the built image.
///
/// Grammar:
/// - Starts with `/`.
/// - No NUL byte.
/// - No `\` (backslash) — UMF builds Linux images.
/// - No `//` runs (collapsed at parse time? — no, rejected so the source
///   stays a 1:1 representation of what was written).
/// - No trailing `/` except at the root.
/// - Each non-root component is non-empty.
///
/// This is purely syntactic — we don't verify the path exists or is
/// well-formed against the staging tree; that's the builder's job.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AbsolutePath(String);

impl AbsolutePath {
    /// Parse and validate an absolute path.
    ///
    /// # Errors
    /// Returns an [`AbsolutePathError`] when the input violates the
    /// grammar.
    pub fn new(raw: impl Into<String>) -> Result<Self, AbsolutePathError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(AbsolutePathError::Empty);
        }
        if !raw.starts_with('/') {
            let ch = raw.chars().next().unwrap_or('?');
            return Err(AbsolutePathError::NotAbsolute { ch });
        }
        if let Some(pos) = raw.find('\0') {
            return Err(AbsolutePathError::NulByte { offset: pos });
        }
        if let Some(pos) = raw.find('\\') {
            return Err(AbsolutePathError::Backslash { offset: pos });
        }
        // Trailing `/` only allowed when the whole path *is* `/`.
        if raw.len() > 1 && raw.ends_with('/') {
            return Err(AbsolutePathError::TrailingSlash {
                offset: raw.len() - 1,
            });
        }
        // Empty components (`//`) are rejected so the path stays exactly as
        // the user wrote it; the builder doesn't have to defensively normalize.
        let mut prev_slash = false;
        for (i, ch) in raw.char_indices() {
            if ch == '/' {
                if prev_slash {
                    return Err(AbsolutePathError::EmptyComponent { offset: i });
                }
                prev_slash = true;
            } else {
                prev_slash = false;
            }
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrow as a [`Path`] for consumers that want filesystem operations.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl AsRef<str> for AbsolutePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for AbsolutePath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for AbsolutePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an [`AbsolutePath`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AbsolutePathError {
    /// The input is empty.
    #[error("path cannot be empty")]
    Empty,
    /// The input does not start with `/`.
    #[error("path must be absolute (start with `/`), found `{ch}`")]
    NotAbsolute {
        /// The actual leading character.
        ch: char,
    },
    /// The path contains a NUL byte.
    #[error("path cannot contain a NUL byte")]
    NulByte {
        /// Byte offset of the NUL.
        offset: usize,
    },
    /// The path contains a backslash (UMF targets Linux only).
    #[error("path cannot contain `\\` — UMF images are Linux")]
    Backslash {
        /// Byte offset of the backslash.
        offset: usize,
    },
    /// The path ends with a `/` other than at the root.
    #[error("path cannot end with `/` (except the root `/`)")]
    TrailingSlash {
        /// Byte offset of the trailing slash.
        offset: usize,
    },
    /// The path contains a `//` run (empty component).
    #[error("path cannot contain an empty component (consecutive `/`)")]
    EmptyComponent {
        /// Byte offset of the second `/`.
        offset: usize,
    },
}

impl ValidationError for AbsolutePathError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::NotAbsolute { .. } => Some(0),
            Self::NulByte { offset }
            | Self::Backslash { offset }
            | Self::TrailingSlash { offset }
            | Self::EmptyComponent { offset } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some("paths in the image are absolute and use `/`, e.g. `/etc/nginx/nginx.conf`")
    }
}

/// A permissive filesystem path as written in a UMF recipe.
///
/// Targets `WORKDIR`, `ADD`/`COPY` destinations, and `RUN --mount target` —
/// the directives where recipe authors expect to write paths freely.
/// Stored verbatim so the builder's downstream processing sees exactly
/// what the author wrote — in particular, a trailing `/` on an ADD/COPY
/// destination forces directory semantics on the target and that signal
/// must not be silently stripped.
///
/// Grammar:
/// - Non-empty.
/// - No NUL byte.
/// - No `\` (backslash) — UMF builds Linux images.
///
/// Everything else is accepted: relative paths (no leading `/`), trailing
/// `/`, `//` runs, `.` / `..` components. Semantic resolution (joining a
/// relative path against the current `WORKDIR`, collapsing dots, etc.) is
/// the builder's job, not the parser's.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecipePath(String);

impl RecipePath {
    /// Parse and validate a recipe path.
    ///
    /// # Errors
    /// Returns a [`RecipePathError`] when the input violates the grammar.
    pub fn new(raw: impl Into<String>) -> Result<Self, RecipePathError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(RecipePathError::Empty);
        }
        if let Some(pos) = raw.find('\0') {
            return Err(RecipePathError::NulByte { offset: pos });
        }
        if let Some(pos) = raw.find('\\') {
            return Err(RecipePathError::Backslash { offset: pos });
        }
        Ok(Self(raw))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrow as a [`Path`] for consumers that want filesystem operations.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Whether the path starts with `/`.
    #[must_use]
    pub fn is_absolute(&self) -> bool {
        self.0.starts_with('/')
    }

    /// Whether the path ends with `/` (and is not the root `/`).
    ///
    /// A trailing slash forces directory interpretation on ADD/COPY
    /// destinations.
    #[must_use]
    pub fn has_trailing_slash(&self) -> bool {
        self.0.len() > 1 && self.0.ends_with('/')
    }
}

impl AsRef<str> for RecipePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for RecipePath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for RecipePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`RecipePath`] failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecipePathError {
    /// The input is empty.
    #[error("path cannot be empty")]
    Empty,
    /// The path contains a NUL byte.
    #[error("path cannot contain a NUL byte")]
    NulByte {
        /// Byte offset of the NUL.
        offset: usize,
    },
    /// The path contains a backslash (UMF targets Linux only).
    #[error("path cannot contain `\\` — UMF images are Linux")]
    Backslash {
        /// Byte offset of the backslash.
        offset: usize,
    },
}

impl ValidationError for RecipePathError {
    fn offset(&self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::NulByte { offset } | Self::Backslash { offset } => Some(*offset),
        }
    }

    fn hint(&self) -> Option<&'static str> {
        Some(
            "paths may be absolute (`/etc/nginx`) or relative to the current WORKDIR (`./conf`); a trailing `/` is allowed and forces directory semantics on ADD/COPY destinations",
        )
    }
}

#[cfg(test)]
mod tests;
