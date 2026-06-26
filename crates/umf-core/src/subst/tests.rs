//! Unit tests for build-time variable substitution.

use std::collections::BTreeMap;

use super::{contains_placeholder, substitute};

/// Build a lookup closure from name→value pairs.
fn scope(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn sub(input: &str, pairs: &[(&str, &str)]) -> String {
    let map = scope(pairs);
    substitute(input, |n| map.get(n).map(String::as_str))
}

#[test]
fn expands_braced_and_bare_forms() {
    assert_eq!(sub("v${VERSION}", &[("VERSION", "1.0")]), "v1.0");
    assert_eq!(sub("v$VERSION", &[("VERSION", "1.0")]), "v1.0");
    assert_eq!(
        sub("myapp:${TAG}-$ARCH", &[("TAG", "1.0"), ("ARCH", "amd64")]),
        "myapp:1.0-amd64"
    );
}

#[test]
fn unknown_names_are_left_verbatim_never_blanked() {
    // The whole point: an undeclared reference passes through untouched so the
    // shell (for a RUN) sees it, rather than silently expanding to empty.
    assert_eq!(sub("echo $HOME", &[]), "echo $HOME");
    assert_eq!(sub("echo ${UNDECLARED}", &[]), "echo ${UNDECLARED}");
    assert_eq!(
        sub("a $KNOWN b $UNKNOWN", &[("KNOWN", "x")]),
        "a x b $UNKNOWN"
    );
}

#[test]
fn shell_constructs_pass_through() {
    // `$(...)` command substitution and arithmetic must not be touched.
    assert_eq!(sub("$(date +%s)", &[("date", "nope")]), "$(date +%s)");
    assert_eq!(sub("$((1 + 2))", &[]), "$((1 + 2))");
    // A lone trailing `$`.
    assert_eq!(sub("cost is 5$", &[]), "cost is 5$");
}

#[test]
fn full_identifier_match_no_prefix_confusion() {
    // `$VERSION` must not match the value bound to `$VER`.
    assert_eq!(
        sub("$VERSIONED", &[("VERSION", "1.0"), ("VERSIONED", "z")]),
        "z"
    );
    assert_eq!(sub("$VER_SION", &[("VER", "1.0")]), "$VER_SION");
}

#[test]
fn malformed_braced_is_left_verbatim() {
    assert_eq!(sub("${UNCLOSED", &[("UNCLOSED", "x")]), "${UNCLOSED");
    assert_eq!(sub("${}", &[]), "${}");
    assert_eq!(sub("${1BAD}", &[("1BAD", "x")]), "${1BAD}");
}

#[test]
fn substituted_value_is_not_rescanned() {
    // A value that itself looks like a reference is emitted literally — no
    // recursive expansion, no loops.
    assert_eq!(sub("$A", &[("A", "$B"), ("B", "boom")]), "$B");
}

#[test]
fn no_dollar_is_identity() {
    assert_eq!(sub("plain text 123", &[("X", "y")]), "plain text 123");
}

#[test]
fn contains_placeholder_detects_both_forms() {
    assert!(contains_placeholder("myapp:${VERSION}"));
    assert!(contains_placeholder("myapp:$VERSION"));
    assert!(contains_placeholder("a${B}c"));
}

#[test]
fn contains_placeholder_ignores_non_placeholders() {
    assert!(!contains_placeholder("myapp:1.0"));
    assert!(!contains_placeholder("price 5$"));
    assert!(!contains_placeholder("$(date)"));
    assert!(!contains_placeholder("$ "));
    assert!(!contains_placeholder(""));
}
