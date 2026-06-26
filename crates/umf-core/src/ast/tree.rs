//! Top-level AST: a parsed UMF source file is an [`Ast`] of [`Stage`]s.

use super::{Arg, Directive, Span, Spanned};
use crate::types::{OciReference, StageName};

/// A parsed UMF source file.
///
/// Holds one or more [`Stage`]s in declaration order. A non-multi-stage file
/// has exactly one stage; multi-stage files have additional stages referenced
/// from later stages via `ADD --from=<name>`.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ast {
    /// Build-global `ARG` declarations that appear *before* the first `FROM`
    ///. `ARG` is the only directive permitted there; a pre-`FROM`
    /// `ARG` is global to the build and may be referenced in a `FROM` line.
    /// Empty for a file with no pre-`FROM` preamble.
    pub global_args: Vec<Arg>,
    /// Stages in source-declaration order.
    pub stages: Vec<Stage>,
}

/// One stage of a multi-stage build.
///
/// A stage begins with a `FROM` directive (captured in [`Stage::from`]) and
/// continues with the directives that operate on that L0.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// The L0 reference — `FROM <ref>` or `FROM scratch`.
    pub from: FromArg,
    /// Optional `AS <name>` alias (used by `ADD --from=<name>` in later stages).
    pub name: Option<Spanned<StageName>>,
    /// All directives in this stage, in source order. The `FROM` itself is not
    /// included here — it's captured in [`Stage::from`].
    pub directives: Vec<Directive>,
    /// Source span covering the entire stage.
    pub span: Span,
}

/// The argument of a `FROM` directive — what serves as L0.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromArg {
    /// Where L0 comes from.
    pub source: FromSource,
    /// Source span covering the entire `FROM <arg> [AS <name>]` directive.
    pub span: Span,
}

/// Where L0 originates.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FromSource {
    /// `FROM scratch` — blank L0, unlocks the full boot chain.
    Scratch,
    /// `FROM <image-ref>` — pulls an OCI artifact (or, at semantic-resolution
    /// time, names a prior stage). The parser does not distinguish these two —
    /// disambiguation happens during semantic validation.
    Reference(Spanned<OciReference>),
}
