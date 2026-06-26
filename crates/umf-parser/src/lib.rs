//! UMF source → AST.
//!
//! Public entrypoint is [`parse`]. The pipeline is:
//!
//! 1. [`lexer::tokenize`] — byte stream → tokens
//! 2. [`grammar::parse`] — tokens → [`umf_core::ast::Ast`]
//! 3. [`validator::validate`] — structural / semantic checks over the AST
//!
//! Diagnostics from all three stages are aggregated and returned together so
//! tooling (LSPs, linters) can report multiple problems from one pass.
//!
//! Depends only on `umf-core` so the parser can be pulled into future tools
//! without dragging builder-side dependencies along.

use umf_core::ast::{Ast, Span};

pub mod diagnostics;
pub mod grammar;
pub mod lexer;
pub mod validator;

use diagnostics::{Annotation, Diagnostic};

/// Maximum UMF source size the parser accepts, in bytes (8 MiB).
///
/// Recipes are directive lists measured in kilobytes, so this cap is far above
/// any legitimate input. It exists purely to bound memory: the lexer/grammar
/// allocate proportionally to the input (cloned token text), so without a cap a
/// multi-gigabyte source — recipes can arrive as OCI artifacts and be parsed
/// server-side — would OOM the process. Over-cap input is rejected with a
/// diagnostic instead.
pub const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// Parse a UMF source string into an [`Ast`] plus any non-fatal warnings,
/// running lex + grammar + semantic validation.
///
/// All three phases' diagnostics are concatenated (lex first, then parse, then
/// validate). A successful return means the source is both syntactically *and*
/// structurally sound; the accompanying `Vec<Diagnostic>` carries any
/// **warnings** (recognized-but-unsupported directives such as `ARG` / `CMD`,
/// and `${…}` references umf does not substitute) for the caller to render. A
/// failed return contains every error found across all phases so a caller can
/// render them all in one pass.
///
/// # Errors
/// Returns a non-empty `Vec<Diagnostic>` when the source contains any lexical,
/// grammatical, or semantic errors. Render each one with
/// [`diagnostics::report`].
#[tracing::instrument(level = "info", name = "umf.parse", skip(source), fields(source.bytes = source.len()))]
pub fn parse_with_warnings(source: &str) -> Result<(Ast, Vec<Diagnostic>), Vec<Diagnostic>> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(vec![
            Diagnostic::error(
                format!(
                    "recipe is {} bytes, exceeding the {MAX_SOURCE_BYTES}-byte parser limit",
                    source.len(),
                ),
                Annotation::new(Span::new(0, 1), "input exceeds the maximum recipe size"),
            )
            .with_hint(
                "reduce the recipe size; this cap bounds memory against accidental or \
                 hostile oversized inputs",
            ),
        ]);
    }
    let (tokens, mut diagnostics) = lexer::tokenize(source);
    match grammar::parse(source, tokens) {
        Ok((ast, warnings)) => {
            let mut val_errors = validator::validate(&ast);
            diagnostics.append(&mut val_errors);
            if diagnostics.is_empty() {
                Ok((ast, warnings))
            } else {
                Err(diagnostics)
            }
        }
        Err(mut parse_errors) => {
            diagnostics.append(&mut parse_errors);
            Err(diagnostics)
        }
    }
}

/// Parse a UMF source string into an [`Ast`], discarding any non-fatal
/// warnings.
///
/// A thin convenience wrapper over [`parse_with_warnings`] for callers that
/// only need the AST (tests and internal tooling). User-facing entrypoints
/// (`umf parse`, `umf build`) call [`parse_with_warnings`] and render the
/// returned warnings to stderr, so unsupported directives and unsubstituted
/// `${…}` references are never silent.
///
/// # Errors
/// Returns a non-empty `Vec<Diagnostic>` when the source contains any lexical,
/// grammatical, or semantic errors. Render each one with
/// [`diagnostics::report`].
pub fn parse(source: &str) -> Result<Ast, Vec<Diagnostic>> {
    parse_with_warnings(source).map(|(ast, _warnings)| ast)
}

#[cfg(test)]
mod tests;
