//! Unit tests for the `lexer` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

fn kinds(source: &str) -> Vec<TokenKind> {
    let (tokens, errors) = tokenize(source);
    assert!(errors.is_empty(), "unexpected lex errors: {errors:?}");
    tokens.into_iter().map(|t| t.kind).collect()
}

#[test]
fn keywords_recognized_uppercase_only() {
    let ks = kinds("FROM scratch\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::From),
            TokenKind::Ident("scratch".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn references_with_colons_are_one_token() {
    let ks = kinds("ROOTFS debian:bookworm\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Rootfs),
            TokenKind::Ident("debian:bookworm".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn paths_with_slashes_are_one_token() {
    let ks = kinds("ADD ./certs/myorg-ca.crt /usr/local/share/myorg-ca.crt\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Add),
            TokenKind::Ident("./certs/myorg-ca.crt".to_string()),
            TokenKind::Ident("/usr/local/share/myorg-ca.crt".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn expose_splits_port_from_protocol() {
    let ks = kinds("EXPOSE 80/tcp\n");
    // The lexer emits the protocol as `/tcp` (one ident); EXPOSE's grammar
    // peels the leading `/` off.
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Expose),
            TokenKind::Number(80),
            TokenKind::Ident("/tcp".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn label_with_equals_is_three_tokens() {
    let ks = kinds("LABEL org.imagilux.umf.type=kernel\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Label),
            TokenKind::Ident("org.imagilux.umf.type".to_string()),
            TokenKind::Punct(Punct::Equals),
            TokenKind::Ident("kernel".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn quoted_string_with_escapes() {
    let ks = kinds(
        r#"LABEL author="Jane \"J\" Doe"
"#,
    );
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Label),
            TokenKind::Ident("author".to_string()),
            TokenKind::Punct(Punct::Equals),
            TokenKind::String(r#"Jane "J" Doe"#.to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn quoted_string_preserves_multibyte_utf8() {
    // The old `byte as char` push reinterpreted each UTF-8 byte as
    // Latin-1, mangling any non-ASCII quoted value.
    let ks = kinds("LABEL author=\"José 🚀 café\"\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Label),
            TokenKind::Ident("author".to_string()),
            TokenKind::Punct(Punct::Equals),
            TokenKind::String("José 🚀 café".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn long_option_with_value() {
    let ks = kinds("ADD --from=builder /app /target\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Keyword(Keyword::Add),
            TokenKind::LongOption {
                name: "from".to_string(),
                value: Some("builder".to_string()),
            },
            TokenKind::Ident("/app".to_string()),
            TokenKind::Ident("/target".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn line_continuation_joins_lines() {
    let ks = kinds("RUN apt-get update && \\\n    apt-get install -y nginx\n");
    // `\\\n` is consumed and produces no newline; we get one logical line.
    let newlines = ks
        .iter()
        .filter(|k| matches!(k, TokenKind::Newline))
        .count();
    assert_eq!(
        newlines, 1,
        "line continuation should collapse to 1 newline"
    );
}

#[test]
fn crlf_line_continuation_joins_lines() {
    // A Windows-authored recipe ends continued lines with `\\\r\n`. `kinds`
    // asserts there are zero lex errors, so this fails outright if the lone
    // backslash falls through to the `unexpected character` catch-all.
    let ks = kinds("RUN apt-get update && \\\r\n    apt-get install -y nginx\r\n");
    let newlines = ks
        .iter()
        .filter(|k| matches!(k, TokenKind::Newline))
        .count();
    assert_eq!(
        newlines, 1,
        "CRLF line continuation should collapse to 1 newline"
    );
}

#[test]
fn oversized_numeric_literal_is_an_error() {
    // A digit run wider than u64 clamps to u64::MAX but must report a
    // diagnostic rather than silently changing the author's value.
    let (tokens, errors) = tokenize("EXPOSE 99999999999999999999999\n");
    assert_eq!(errors.len(), 1, "expected exactly one overflow error");
    assert!(
        tokens.iter().any(|t| t.kind == TokenKind::Number(u64::MAX)),
        "the literal should still be emitted, clamped to u64::MAX"
    );
}

#[test]
fn comments_are_skipped() {
    let ks = kinds("# leading comment\nFROM scratch # trailing comment\n");
    assert_eq!(
        ks,
        vec![
            TokenKind::Newline, // the blank-after-comment newline
            TokenKind::Keyword(Keyword::From),
            TokenKind::Ident("scratch".to_string()),
            TokenKind::Newline,
        ]
    );
}

#[test]
fn unterminated_string_is_an_error() {
    let (_, errors) = tokenize(
        r#"LABEL author="unterminated
"#,
    );
    assert_eq!(errors.len(), 1, "expected exactly one error");
}
