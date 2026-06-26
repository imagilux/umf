//! Semantic validation pass over an [`Ast`].
//!
//! Catches the structural mistakes a syntactically-clean AST can still
//! contain — duplicate single-instance directives and stage-name collisions.
//!
//! What's **deliberately not** done here:
//!
//! - L0 introspection (reading `org.imagilux.umf.type` from the resolved
//!   `FROM` artifact). That's a registry/IO operation handled by the builder
//!   — the validator here can only see source. So a bootable build
//!   whose `FROM` resolves to a non-kernel image passes the validator, but the
//!   builder rejects it when it introspects the label and finds type ≠ kernel.
//! - `ADD --from=<stage>` reference checking — confirming the named stage
//!   exists. The grammar parses `--from` (stage name or image ref), but
//!   cross-stage existence isn't validated here yet.
//!
//! The checks here are pure functions over the AST and require no IO.

use std::collections::HashMap;

use umf_core::ast::{Ast, Directive, Span, Stage};

use crate::diagnostics::{Annotation, Diagnostic};

/// Run the semantic validation pass over `ast`. Returns one [`Diagnostic`] per
/// distinct problem found. Empty vector ⇒ the AST is structurally sound.
#[must_use]
pub fn validate(ast: &Ast) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    check_stage_name_uniqueness(ast, &mut diagnostics);
    for stage in &ast.stages {
        validate_stage(stage, &mut diagnostics);
    }
    diagnostics
}

// ----- cross-stage checks -----

fn check_stage_name_uniqueness(ast: &Ast, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: HashMap<&str, Span> = HashMap::new();
    for stage in &ast.stages {
        let Some(name) = &stage.name else { continue };
        if let Some(&first_span) = seen.get(name.value.as_str()) {
            diagnostics.push(
                Diagnostic::error(
                    format!("duplicate stage name `{}`", name.value),
                    Annotation::new(name.span, "redeclared here"),
                )
                .with_note(Annotation::new(first_span, "first declared here"))
                .with_hint("stage names must be unique within a multi-stage build"),
            );
        } else {
            seen.insert(name.value.as_str(), name.span);
        }
    }
}

// ----- per-stage checks -----

fn validate_stage(stage: &Stage, diagnostics: &mut Vec<Diagnostic>) {
    check_no_duplicate_singletons(stage, diagnostics);
}

/// Reject more than one of any directive that's structurally singular.
///
/// Per `docs/specification.md`, the only structurally-singular directive is
/// ENTRYPOINT (at most one per stage). Every other directive (LABEL, ENV, ARG,
/// RUN, ADD, COPY, EXPOSE, SHELL, USER, WORKDIR, CMD, VOLUME, STOPSIGNAL) may
/// repeat — last-wins or accumulate per its own semantics — so none is checked
/// here.
fn check_no_duplicate_singletons(stage: &Stage, diagnostics: &mut Vec<Diagnostic>) {
    let mut firsts: HashMap<SingletonKind, Span> = HashMap::new();
    for directive in &stage.directives {
        let Some(kind) = singleton_kind(directive) else {
            continue;
        };
        let span = directive.span();
        if let Some(&first_span) = firsts.get(&kind) {
            diagnostics.push(
                Diagnostic::error(
                    format!("duplicate {} directive in stage", kind.label()),
                    Annotation::new(span, "duplicate here"),
                )
                .with_note(Annotation::new(first_span, "first declared here"))
                .with_hint(format!(
                    "{} accepts at most one instance per stage",
                    kind.label()
                )),
            );
        } else {
            firsts.insert(kind, span);
        }
    }
}

// Bootable / kernel coherence (e.g. `FROM scratch` can't be bootable) is checked
// at build time by the builder's L0 introspection, which can read the resolved
// `FROM` artifact's `type` label — the parser sees only source text and can't.

// ----- internal classification helpers -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SingletonKind {
    Entrypoint,
}

impl SingletonKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Entrypoint => "ENTRYPOINT",
        }
    }
}

const fn singleton_kind(d: &Directive) -> Option<SingletonKind> {
    match d {
        Directive::Entrypoint(_) => Some(SingletonKind::Entrypoint),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
