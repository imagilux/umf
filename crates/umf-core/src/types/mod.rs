//! Validated newtypes for the values carried in directive arguments.
//!
//! The AST keeps these in place of bare `Spanned<String>` so structural
//! mistakes (a malformed env var name, a label key starting with a digit,
//! a service unit with an unknown suffix) are caught at parse time and
//! turned into source-spanned diagnostics rather than surfacing later in
//! the builder.
//!
//! Each newtype:
//!
//! - Has a fallible `new` constructor that runs the syntax check.
//! - Implements [`AsRef<str>`], [`Display`](std::fmt::Display), and (via the
//!   newtype's own `as_str`) gives consumers the underlying string.
//! - Pairs with a [`ValidationError`]-implementing error enum carrying the
//!   byte offset of the problem so the parser can emit a precise sub-span.
//!
//! The newtypes are pure syntax: they never touch the filesystem, the network,
//! or any external database. Semantic checks that need additional context
//! (e.g. confirming a stage name referenced by `ADD --from` actually exists)
//! continue to live in the validator.

use std::error::Error;

pub mod identifier;
pub mod oci;
pub mod path;
pub mod url;

pub use identifier::{
    EnvVarName, EnvVarNameError, EnvVarValue, EnvVarValueError, LabelKey, LabelKeyError,
    LabelValue, LabelValueError, SecretId, SecretIdError, ServiceUnitName, ServiceUnitNameError,
    StageName, StageNameError, UnitSuffix, Username, UsernameError,
};
pub use oci::{OciReference, OciReferenceError};
pub use path::{AbsolutePath, AbsolutePathError, RecipePath, RecipePathError};
pub use url::{HttpsUrl, HttpsUrlError};

/// A validation error that knows where in its input string the problem started.
///
/// Implementors are used by the parser to build sub-spans for diagnostics: the
/// final span starts at the token's `span.start + offset()` and is one character
/// long when an offset is reported. When [`offset`](ValidationError::offset)
/// returns [`None`] the parser falls back to the entire token's span.
pub trait ValidationError: Error {
    /// Byte offset inside the input string where the problem starts, if any.
    ///
    /// `None` for whole-input failures such as `Empty` or a length-cap miss
    /// that doesn't have a single offending position.
    fn offset(&self) -> Option<usize>;

    /// Optional terse hint surfaced as the diagnostic's `help:` line.
    fn hint(&self) -> Option<&'static str> {
        None
    }
}
