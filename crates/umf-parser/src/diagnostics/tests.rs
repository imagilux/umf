//! Unit tests for the `diagnostics` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn error_diagnostic_renders_without_panic() {
    let source = "FROM scratch\nKERNEL linux:v7.0\n";
    let diag = Diagnostic::error(
        "boot-chain directive in unsupported context",
        Annotation::new(
            Span::new(13, 30),
            "this directive is not allowed in a container-shaped build",
        ),
    )
    .with_note(Annotation::new(
        Span::new(0, 12),
        "container L0 is inferred from this FROM",
    ))
    .with_hint("either use `FROM scratch` and a full boot chain, or drop the KERNEL directive");

    let mut out = Vec::new();
    report(&diag, &mut out, "test.umf", source).expect("render should succeed");

    let output = String::from_utf8_lossy(&out);
    assert!(output.contains("boot-chain directive"));
    assert!(output.contains("test.umf"));
}

#[test]
fn diagnostic_chaining_collects_notes_and_hint() {
    let primary = Annotation::new(Span::new(0, 5), "primary");
    let note1 = Annotation::new(Span::new(10, 15), "note 1");
    let note2 = Annotation::new(Span::new(20, 25), "note 2");

    let diag = Diagnostic::warning("test warning", primary)
        .with_note(note1)
        .with_note(note2)
        .with_hint("try this");

    assert_eq!(diag.severity, Severity::Warning);
    assert_eq!(diag.notes.len(), 2);
    assert_eq!(diag.hint.as_deref(), Some("try this"));
}

#[test]
fn report_all_caps_rendered_diagnostics() {
    // A pathological input can produce thousands of lexer errors; rendering
    // each one is quadratic in the source size and floods the terminal.
    // `report_all` must cap the count.
    let source = "FROM scratch\n";
    let diags: Vec<Diagnostic> = (0..MAX_RENDERED_DIAGNOSTICS * 4)
        .map(|_| Diagnostic::error("boom", Annotation::new(Span::new(0, 4), "here")))
        .collect();

    let mut out = Vec::new();
    report_all(&diags, &mut out, "test.umf", source).expect("render");
    let text = String::from_utf8_lossy(&out);

    assert_eq!(
        text.matches("boom").count(),
        MAX_RENDERED_DIAGNOSTICS,
        "must render exactly the cap, not all diagnostics"
    );
    assert!(
        text.contains("more diagnostic"),
        "must note the truncated remainder"
    );
}
