//! Source spans for AST nodes.

use std::ops::Deref;

/// A byte range into the source text.
///
/// Half-open: `start` is inclusive, `end` is exclusive — same convention as
/// Rust string slicing and most diagnostic crates.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Byte offset of the first character (inclusive).
    pub start: usize,
    /// Byte offset one past the last character (exclusive).
    pub end: usize,
}

impl Span {
    /// Construct a span covering bytes `start..end`.
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Length of the span in bytes.
    ///
    /// Saturates at zero for a malformed (`end < start`) span rather than
    /// underflowing.
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Is the span empty (zero bytes)?
    pub const fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Pairs a value with its source [`Span`].
///
/// Use this for user-controlled values (strings, identifiers) where a future
/// diagnostic might want to highlight just the value rather than the entire
/// enclosing directive. Composite AST nodes carry their span inline as a
/// `span` field instead.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spanned<T> {
    /// The wrapped value.
    pub value: T,
    /// The source span of the value.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Wrap `value` with its `span`.
    pub const fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }
}

impl<T> Deref for Spanned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}
