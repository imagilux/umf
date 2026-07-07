//! Tokenizer for UMF source.
//!
//! Byte-level scanner producing a [`Vec<Token>`]. Handles:
//!
//! - Directive keywords (FROM, RUN, ADD, …) — see [`Keyword`].
//! - Identifiers / references — `linux:v7.0`, `myorg-ca.crt`, `/usr/local/bin`.
//!   Word chars are alphanumeric plus `_ - . : / *`, so colon-separated refs
//!   and slash-containing paths lex as a single token.
//! - Quoted strings (`"..."` and `'...'`) with `\n \t \r \\ \" \'` escapes.
//! - Unsigned integers.
//! - Punctuation: `= , [ ]` — used by `LABEL key=value`, EXPOSE's port split,
//!   and RUN's exec-form list.
//! - Long options: `--name[=value]` — the value runs until the next whitespace.
//! - Comments: `#` to end of line; skipped.
//! - Line continuation: `\\\n` (or `\\\r\n` for CRLF-authored sources) joins the
//!   next physical line into the current logical line (no [`TokenKind::Newline`]
//!   emitted at the `\\`).
//! - Newlines: emit [`TokenKind::Newline`]; the grammar uses these to know
//!   when a directive ends.
//!
//! Lexical errors (unterminated string, unexpected character) are collected
//! into [`Diagnostic`]s and returned alongside whatever tokens were successfully
//! scanned, so downstream stages can still produce a best-effort parse for
//! diagnostics-heavy IDEs.

use umf_core::ast::Span;

use crate::diagnostics::{Annotation, Diagnostic};

/// A lexed token: kind + source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Source span of the token in the original input.
    pub span: Span,
}

/// All token kinds emitted by [`tokenize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// A directive keyword (`FROM`, `RUN`, …) or `AS`.
    Keyword(Keyword),
    /// A bare identifier / reference (`linux:v7.0`, `tcp`, `/etc/foo`).
    Ident(String),
    /// A quoted string literal (the wrapped value has quotes stripped and
    /// backslash escapes resolved).
    String(String),
    /// An unsigned integer literal.
    Number(u64),
    /// A single punctuation character.
    Punct(Punct),
    /// `--name[=value]` long option (used by `RUN --mount=…` and `ADD --from=…`).
    LongOption {
        /// The option name (everything between `--` and `=` or end-of-token).
        name: String,
        /// The optional value after `=`, run until next whitespace.
        value: Option<String>,
    },
    /// End of a logical line. Emitted at each physical newline that isn't
    /// preceded by a line-continuation `\\`.
    Newline,
}

/// Punctuation tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Punct {
    /// `=`
    Equals,
    /// `,`
    Comma,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
}

/// Directive-keyword tokens.
///
/// Recognized case-sensitively in uppercase (matching the spec's convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    /// `FROM`
    From,
    /// `AS` (only valid inside a `FROM ... AS <name>` clause).
    As,
    /// `LABEL`
    Label,
    /// `ENV`
    Env,
    /// `ARG`
    Arg,
    /// `BOOTLOADER`
    Bootloader,
    /// `ROOTFS`
    Rootfs,
    /// `SHELL`
    Shell,
    /// `USER`
    User,
    /// `WORKDIR`
    Workdir,
    /// `RUN`
    Run,
    /// `ADD`
    Add,
    /// `ENTRYPOINT`
    Entrypoint,
    /// `EXPOSE`
    Expose,
    /// `ENABLE`
    Enable,
    /// `DISABLE`
    Disable,
    /// `HOSTNAME`
    Hostname,
    /// `LOCALE`
    Locale,
    /// `TIMEZONE`
    Timezone,
    /// `CMD`
    Cmd,
    /// `VOLUME`
    Volume,
    /// `STOPSIGNAL`
    Stopsignal,
    /// `COPY` (routed to `ADD` in plain-copy mode).
    Copy,
}

impl Keyword {
    /// Look up an uppercase string as a keyword. Returns `None` for anything
    /// not in the keyword set. (Named `lookup` rather than `from_str` to
    /// avoid shadowing [`std::str::FromStr`]; UMF's keyword set is closed and
    /// finite, so a `FromStr` impl would be misleading.)
    pub fn lookup(s: &str) -> Option<Self> {
        Some(match s {
            "FROM" => Self::From,
            "AS" => Self::As,
            "LABEL" => Self::Label,
            "ENV" => Self::Env,
            "ARG" => Self::Arg,
            "BOOTLOADER" => Self::Bootloader,
            "ROOTFS" => Self::Rootfs,
            "SHELL" => Self::Shell,
            "USER" => Self::User,
            "WORKDIR" => Self::Workdir,
            "RUN" => Self::Run,
            "ADD" => Self::Add,
            "ENTRYPOINT" => Self::Entrypoint,
            "EXPOSE" => Self::Expose,
            "ENABLE" => Self::Enable,
            "DISABLE" => Self::Disable,
            "HOSTNAME" => Self::Hostname,
            "LOCALE" => Self::Locale,
            "TIMEZONE" => Self::Timezone,
            "CMD" => Self::Cmd,
            "VOLUME" => Self::Volume,
            "STOPSIGNAL" => Self::Stopsignal,
            "COPY" => Self::Copy,
            _ => return None,
        })
    }
}

/// Tokenize a UMF source string.
///
/// Always returns the [`Vec<Token>`] of whatever was successfully scanned plus a
/// [`Vec<Diagnostic>`] of any lexical errors. Empty diagnostics ⇒ clean lex.
pub fn tokenize(source: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lexer = Lexer::new(source);
    lexer.run();
    (lexer.tokens, lexer.errors)
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    errors: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => self.emit(TokenKind::Newline, 1),
                b'\\' if self.peek(1) == Some(b'\n') => {
                    // Line continuation (LF): drop the backslash AND the newline.
                    self.pos += 2;
                }
                b'\\' if self.peek(1) == Some(b'\r') && self.peek(2) == Some(b'\n') => {
                    // Line continuation (CRLF): drop the backslash, CR, AND the LF,
                    // so Windows-authored recipes join lines just like LF ones.
                    self.pos += 3;
                }
                b'#' => self.skip_comment(),
                b'"' | b'\'' => self.lex_string(c),
                b'=' => self.emit(TokenKind::Punct(Punct::Equals), 1),
                b',' => self.emit(TokenKind::Punct(Punct::Comma), 1),
                b'[' => self.emit(TokenKind::Punct(Punct::LBracket), 1),
                b']' => self.emit(TokenKind::Punct(Punct::RBracket), 1),
                b'-' if self.peek(1) == Some(b'-') => self.lex_long_option(),
                c if c.is_ascii_digit() => self.lex_number(),
                c if is_word_start(c) => self.lex_word(),
                _ => {
                    let span = Span::new(self.pos, self.pos + 1);
                    // Capped: a megabyte of invalid control bytes would otherwise
                    // collect one fat diagnostic per byte (memory-exhaustion DoS).
                    crate::diagnostics::push_capped(
                        &mut self.errors,
                        Diagnostic::error(
                            format!("unexpected character `{}`", c as char),
                            Annotation::new(span, "not allowed here"),
                        )
                        .with_hint("control characters are not allowed here; printable text (including UTF-8) is accepted within words, and arbitrary bytes inside quoted strings"),
                    );
                    self.pos += 1;
                }
            }
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn emit(&mut self, kind: TokenKind, len: usize) {
        let span = Span::new(self.pos, self.pos + len);
        self.tokens.push(Token { kind, span });
        self.pos += len;
    }

    fn skip_comment(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.pos += 1;
        }
        // The newline (if any) is emitted on the next iteration.
    }

    /// Push a recognised escape's resolved character and advance past the
    /// single ASCII escape-indicator byte (`n`, `t`, `\`, `"`, ...).
    fn push_escaped(&mut self, ch: char, value: &mut String) {
        value.push(ch);
        self.pos += 1;
    }

    fn lex_string(&mut self, quote: u8) {
        let start = self.pos;
        self.pos += 1; // opening quote
        let mut value = String::new();
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c == quote {
                self.pos += 1;
                let span = Span::new(start, self.pos);
                self.tokens.push(Token {
                    kind: TokenKind::String(value),
                    span,
                });
                return;
            }
            if c == b'\\' {
                self.pos += 1;
                match self.bytes.get(self.pos) {
                    Some(b'n') => self.push_escaped('\n', &mut value),
                    Some(b't') => self.push_escaped('\t', &mut value),
                    Some(b'r') => self.push_escaped('\r', &mut value),
                    Some(b'\\') => self.push_escaped('\\', &mut value),
                    Some(b'"') => self.push_escaped('"', &mut value),
                    Some(b'\'') => self.push_escaped('\'', &mut value),
                    // Unknown escape: keep the escaped character verbatim,
                    // decoding the full (possibly multi-byte) char from the
                    // source so `\é` isn't split into Latin-1 bytes.
                    Some(_) => {
                        let Some(ch) = self.source[self.pos..].chars().next() else {
                            break;
                        };
                        value.push(ch);
                        self.pos += ch.len_utf8();
                    }
                    None => break,
                }
                continue;
            }
            // Literal char (possibly multi-byte UTF-8): decode the whole char
            // from the source rather than reinterpreting one byte as a Latin-1
            // `char`, which corrupted any non-ASCII quoted value.
            let Some(ch) = self.source[self.pos..].chars().next() else {
                break;
            };
            value.push(ch);
            self.pos += ch.len_utf8();
        }
        // Unterminated string.
        let span = Span::new(start, self.pos);
        crate::diagnostics::push_capped(
            &mut self.errors,
            Diagnostic::error(
                "unterminated string literal",
                Annotation::new(span, "string starts here but never closes"),
            )
            .with_hint(format!("add a closing {} to terminate", quote as char)),
        );
    }

    fn lex_number(&mut self) {
        let start = self.pos;
        let mut n: u64 = 0;
        let mut overflowed = false;
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
            let digit = u64::from(self.bytes[self.pos] - b'0');
            match n.checked_mul(10).and_then(|m| m.checked_add(digit)) {
                Some(next) => n = next,
                None => {
                    // Literal is wider than u64; clamp to u64::MAX but keep
                    // consuming the digit run so the span and lexer position
                    // stay correct, then report it once below.
                    overflowed = true;
                    n = u64::MAX;
                }
            }
            self.pos += 1;
        }
        let span = Span::new(start, self.pos);
        if overflowed {
            crate::diagnostics::push_capped(
                &mut self.errors,
                Diagnostic::error(
                    "integer literal too large",
                    Annotation::new(span, "does not fit in a 64-bit unsigned integer"),
                )
                .with_hint(format!(
                    "the maximum supported value is {}; the literal was clamped to it",
                    u64::MAX
                )),
            );
        }
        self.tokens.push(Token {
            kind: TokenKind::Number(n),
            span,
        });
    }

    fn lex_word(&mut self) {
        let start = self.pos;
        while self.pos < self.bytes.len() && is_word_cont(self.bytes[self.pos]) {
            self.pos += 1;
        }
        let text = &self.source[start..self.pos];
        let span = Span::new(start, self.pos);
        let kind = Keyword::lookup(text)
            .map_or_else(|| TokenKind::Ident(text.to_string()), TokenKind::Keyword);
        self.tokens.push(Token { kind, span });
    }

    fn lex_long_option(&mut self) {
        let start = self.pos;
        self.pos += 2; // skip `--`
        let name_start = self.pos;
        while self.pos < self.bytes.len() && is_word_cont(self.bytes[self.pos]) {
            self.pos += 1;
        }
        let name = self.source[name_start..self.pos].to_string();
        let value = if self.peek(0) == Some(b'=') {
            self.pos += 1;
            let value_start = self.pos;
            while self.pos < self.bytes.len() {
                let c = self.bytes[self.pos];
                if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                    break;
                }
                self.pos += 1;
            }
            Some(self.source[value_start..self.pos].to_string())
        } else {
            None
        };
        let span = Span::new(start, self.pos);
        self.tokens.push(Token {
            kind: TokenKind::LongOption { name, value },
            span,
        });
    }
}

/// Can this byte appear *inside* a word token?
///
/// Defined permissively as "any printable byte that isn't whitespace, a
/// recognized punct (`= , [ ]`), a quote (`" '`), a comment marker (`#`), or
/// the line-continuation backslash". Anything else is part of a word. This is
/// what lets `RUN apt-get update && apt-get install -y nginx` and
/// `RUN echo ${HOME}/bin/foo --flag=value` lex as a sequence of word tokens
/// rather than failing on shell metacharacters — UMF ascribes no meaning to
/// most of these characters; they're opaque payload to the runner.
const fn is_word_cont(c: u8) -> bool {
    !matches!(
        c,
        b' ' | b'\t' | b'\r' | b'\n' | b'=' | b',' | b'[' | b']' | b'"' | b'\'' | b'#' | b'\\'
    ) && c >= 0x20
}

/// Can this byte start a word token? Same alphabet as [`is_word_cont`] minus
/// ASCII digits — digit-leading runs go through [`Lexer::lex_number`] first.
const fn is_word_start(c: u8) -> bool {
    is_word_cont(c) && !c.is_ascii_digit()
}

#[cfg(test)]
mod tests;
