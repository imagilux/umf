//! Build-time variable substitution: `${NAME}` / `$NAME` expansion.
//!
//! Shared by the parser (to detect placeholders in operands that are otherwise
//! validated newtypes, so they parse and defer to build-time resolution) and
//! the builder (to perform the expansion against the resolved `ARG` scope).
//! Pure and IO-free.
//!
//! Two deliberate properties:
//!
//! - **Only known names expand.** A reference whose name is not in scope is
//!   left **verbatim**, never blanked. This protects shell constructs in a
//!   `RUN` line: `$(date)`, `$HOME`, `${UNDECLARED}` all pass through untouched
//!   for the shell to handle, instead of silently becoming the empty string.
//! - **No partial-name matches.** `$NAME` consumes a full identifier
//!   (`[A-Za-z_][A-Za-z0-9_]*`), so `$VER` and `$VERSION` are distinct and a
//!   `$VERSIONED` is not mistaken for `$VERSION`.

/// Whether `c` may start a variable name.
fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

/// Whether `c` may continue a variable name.
fn is_name_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Whether `name` is a well-formed variable name.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if is_name_start(c)) && chars.all(is_name_continue)
}

/// Expand `${NAME}` / `$NAME` references in `input` using `lookup`.
///
/// `lookup(name)` returns the value to substitute, or `None` to leave the
/// reference verbatim. The expansion is single-pass and non-recursive: a
/// substituted value is **not** re-scanned, so a value that itself contains
/// `$X` is emitted literally (matching Docker, and avoiding expansion loops).
pub fn substitute<'v>(input: &str, lookup: impl Fn(&str) -> Option<&'v str>) -> String {
    // Fast path: nothing to do.
    if !input.contains('$') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // `${NAME}`
            Some('{') => {
                chars.next(); // consume `{`
                let mut name = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                match lookup(&name) {
                    Some(val) if closed && is_valid_name(&name) => out.push_str(val),
                    _ => {
                        // Unknown / malformed / unclosed: emit verbatim.
                        out.push_str("${");
                        out.push_str(&name);
                        if closed {
                            out.push('}');
                        }
                    }
                }
            }
            // `$NAME`
            Some(c2) if is_name_start(c2) => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if is_name_continue(nc) {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match lookup(&name) {
                    Some(val) => out.push_str(val),
                    None => {
                        out.push('$');
                        out.push_str(&name);
                    }
                }
            }
            // A lone `$`, `$(`, `$$`, end-of-input, …: literal.
            _ => out.push('$'),
        }
    }
    out
}

/// Whether `input` contains at least one `${NAME}` or `$NAME` placeholder.
///
/// Used by the parser to decide whether an operand that is normally a validated
/// newtype (an OCI reference, a URL) carries a build-time placeholder and must
/// therefore be accepted now and resolved after substitution.
#[must_use]
pub fn contains_placeholder(input: &str) -> bool {
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$'
            && let Some(&next) = chars.peek()
            && (next == '{' || is_name_start(next))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
