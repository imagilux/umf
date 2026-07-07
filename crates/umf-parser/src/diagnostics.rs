//! Diagnostic framework — source-spanned errors / warnings / notes for the
//! parser, the validator, and downstream consumers.
//!
//! Wraps [`ariadne`] to give the parser and the semantic validator a uniform
//! way to emit multi-line, color-aware, source-spanned diagnostics. Each
//! diagnostic carries a primary span (the offending token / directive), any
//! number of secondary spans (related code worth pointing at), and an optional
//! fix hint.
//!
//! The choice of `ariadne` over `codespan-reporting`: `ariadne`'s default
//! rendering reads cleanly without configuration, integrates well with
//! `chumsky` (the parser library), and its arrow-style
//! output makes multi-span diagnostics easier to scan than
//! `codespan-reporting`'s more rigid layout.

use std::io;
use std::io::Write;

use ariadne::{ColorGenerator, Label, Report, ReportKind, Source};
use umf_core::ast::Span;
use umf_core::types::ValidationError;

/// Severity of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// Hard error — the build cannot proceed.
    Error,
    /// Soft warning — the build can proceed but the user should look.
    Warning,
    /// Informational advice — neither an error nor a problem.
    Advice,
}

impl Severity {
    fn to_report_kind(self) -> ReportKind<'static> {
        match self {
            Self::Error => ReportKind::Error,
            Self::Warning => ReportKind::Warning,
            Self::Advice => ReportKind::Advice,
        }
    }
}

/// A source span with an attached short message — used both as a
/// [`Diagnostic`]'s primary annotation and as any related notes attached to it.
#[derive(Debug, Clone)]
pub struct Annotation {
    /// The span being annotated.
    pub span: Span,
    /// Short message describing what's at this span.
    pub message: String,
}

impl Annotation {
    /// Construct an annotation pairing a span with a message.
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

/// A diagnostic — severity + one-line summary + primary annotation + any
/// related-span notes + an optional fix hint.
///
/// Build with [`Diagnostic::error`] / [`Diagnostic::warning`] / [`Diagnostic::advice`],
/// then chain [`with_note`](Diagnostic::with_note) / [`with_hint`](Diagnostic::with_hint),
/// and render with [`report`].
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Severity (`Error`, `Warning`, `Advice`).
    pub severity: Severity,
    /// One-line summary shown at the top of the rendered output.
    pub message: String,
    /// The primary span this diagnostic points at.
    pub primary: Annotation,
    /// Additional related-span notes (rendered alongside the primary).
    pub notes: Vec<Annotation>,
    /// Optional fix hint rendered as a `help:` line at the bottom.
    pub hint: Option<String>,
}

impl Diagnostic {
    /// Construct an error diagnostic with a primary span.
    pub fn error(message: impl Into<String>, primary: Annotation) -> Self {
        Self::new(Severity::Error, message, primary)
    }

    /// Construct a warning diagnostic with a primary span.
    pub fn warning(message: impl Into<String>, primary: Annotation) -> Self {
        Self::new(Severity::Warning, message, primary)
    }

    /// Construct an advice diagnostic with a primary span.
    pub fn advice(message: impl Into<String>, primary: Annotation) -> Self {
        Self::new(Severity::Advice, message, primary)
    }

    fn new(severity: Severity, message: impl Into<String>, primary: Annotation) -> Self {
        Self {
            severity,
            message: message.into(),
            primary,
            notes: Vec::new(),
            hint: None,
        }
    }

    /// Attach a related-span note. Returns `self` for chaining.
    #[must_use]
    pub fn with_note(mut self, note: Annotation) -> Self {
        self.notes.push(note);
        self
    }

    /// Attach a fix hint. Returns `self` for chaining.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Build a [`Diagnostic`] from a [`ValidationError`] produced by a newtype
/// constructor, mapping the error's byte-offset (if any) into a precise
/// sub-span inside `token_span`.
///
/// The diagnostic's primary annotation message echoes the error's `Display`
/// text; the top-level summary is prefixed with `directive` so the reader
/// sees which directive the value came from (e.g. `invalid LABEL key: …`).
pub fn from_validation_error<E: ValidationError>(
    err: &E,
    raw: &str,
    token_span: Span,
    directive: &str,
    arg_label: &str,
) -> Diagnostic {
    let sub_span = match err.offset() {
        Some(offset) if offset < raw.len() => {
            // `offset` is a *byte* offset reported by a newtype validator; for a
            // word carrying non-ASCII bytes (UMF treats UTF-8 as opaque word
            // payload) it can land inside a multi-byte sequence. Snap down to the
            // enclosing char boundary so the slice below can never panic — a
            // parser must not crash on malformed input. (Byte 0 is always a
            // boundary, so this terminates.)
            let mut start = offset;
            while !raw.is_char_boundary(start) {
                start -= 1;
            }
            let absolute_start = token_span.start + start;
            // Point at exactly the char at that offset.
            let ch_len = raw[start..].chars().next().map_or(1, char::len_utf8);
            let absolute_end = (absolute_start + ch_len).min(token_span.end);
            Span::new(absolute_start, absolute_end)
        }
        _ => token_span,
    };
    let message = err.to_string();
    let mut diag = Diagnostic::error(
        format!("invalid {directive} {arg_label}: {message}"),
        Annotation::new(sub_span, message),
    );
    if let Some(hint) = err.hint() {
        diag = diag.with_hint(hint);
    }
    diag
}

/// Render `diagnostic` to `writer`, using `source_name` (typically the file
/// path) as the source label and `source` as the source text.
///
/// # Errors
/// Returns the underlying [`io::Error`] if writing fails.
pub fn report<W: Write>(
    diagnostic: &Diagnostic,
    writer: &mut W,
    source_name: &str,
    source: &str,
) -> io::Result<()> {
    let kind = diagnostic.severity.to_report_kind();
    let primary_range = diagnostic.primary.span.start..diagnostic.primary.span.end;
    let mut colors = ColorGenerator::new();
    let primary_color = colors.next();

    let mut builder = Report::build(kind, (source_name, primary_range.clone()))
        .with_message(&diagnostic.message)
        .with_label(
            Label::new((source_name, primary_range))
                .with_message(&diagnostic.primary.message)
                .with_color(primary_color),
        );

    for note in &diagnostic.notes {
        let note_color = colors.next();
        builder = builder.with_label(
            Label::new((source_name, note.span.start..note.span.end))
                .with_message(&note.message)
                .with_color(note_color),
        );
    }

    if let Some(hint) = &diagnostic.hint {
        builder = builder.with_help(hint);
    }

    builder
        .finish()
        .write((source_name, Source::from(source)), writer)
}

/// Maximum number of diagnostics collected from one parse before further ones
/// are dropped. The renderer only shows [`MAX_RENDERED_DIAGNOSTICS`] anyway;
/// *collecting* millions from a pathological input (e.g. megabytes of invalid
/// control bytes, each a lexer error carrying a formatted message and a long
/// hint string) is a memory-exhaustion DoS — an 8 MiB input can balloon to
/// gigabytes of RSS. This bounds the retained set.
pub const MAX_COLLECTED_DIAGNOSTICS: usize = 256;

/// Push `diag` into `sink` unless it has reached [`MAX_COLLECTED_DIAGNOSTICS`].
/// At the cap it appends a single "further diagnostics suppressed" marker and
/// then drops everything after, bounding memory on hostile input while keeping
/// the first (most informative) errors. Returns `false` once the sink is
/// capped, so a caller in a loop can stop early and also bound *time*.
pub fn push_capped(sink: &mut Vec<Diagnostic>, diag: Diagnostic) -> bool {
    if sink.len() < MAX_COLLECTED_DIAGNOSTICS {
        sink.push(diag);
        return true;
    }
    if sink.len() == MAX_COLLECTED_DIAGNOSTICS {
        sink.push(Diagnostic::error(
            format!(
                "too many diagnostics; further errors suppressed after {MAX_COLLECTED_DIAGNOSTICS}"
            ),
            Annotation::new(Span::new(0, 0), "diagnostic limit reached"),
        ));
    }
    false
}

/// Maximum number of diagnostics [`report_all`] renders; past this it prints a
/// one-line summary instead of more reports.
const MAX_RENDERED_DIAGNOSTICS: usize = 50;

/// Render a batch of diagnostics, capped at `MAX_RENDERED_DIAGNOSTICS`.
///
/// Each [`report`] rebuilds the source index, so rendering *every* diagnostic of
/// a pathological input (e.g. a recipe with thousands of lexer errors) is
/// quadratic in the source size and floods the terminal. Capping the count
/// bounds both — the render cost becomes linear in the source — while still
/// surfacing the first (and usually most informative) errors. Callers should
/// prefer this over looping [`report`] directly on an untrusted diagnostic set.
///
/// # Errors
/// Returns the underlying [`io::Error`] if writing fails.
pub fn report_all<W: Write>(
    diagnostics: &[Diagnostic],
    writer: &mut W,
    source_name: &str,
    source: &str,
) -> io::Result<()> {
    for diagnostic in diagnostics.iter().take(MAX_RENDERED_DIAGNOSTICS) {
        report(diagnostic, writer, source_name, source)?;
    }
    if diagnostics.len() > MAX_RENDERED_DIAGNOSTICS {
        writeln!(
            writer,
            "... and {} more diagnostic(s) (showing the first {})",
            diagnostics.len() - MAX_RENDERED_DIAGNOSTICS,
            MAX_RENDERED_DIAGNOSTICS,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
