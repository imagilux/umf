//! UMF abstract syntax tree.
//!
//! Owned by `umf-core` rather than `umf-parser` so that `umf-builder` can
//! consume ASTs without depending on `umf-parser` — the parser, future
//! linters, and a future LSP server all produce this same shape.
//!
//! Structure:
//! - [`Ast`] — top-level: a list of [`Stage`]s
//! - [`Stage`] — one stage of a multi-stage build (FROM + directives)
//! - [`Directive`] — enum, one variant per spec directive
//! - [`Span`] / [`Spanned`] — byte-offset source tracking for diagnostics

pub mod directive;
pub mod span;
pub mod tree;

pub use directive::{
    Add, AddSource, Arg, Cmd, CmdForm, Directive, Entrypoint, EntrypointInit, Env, Expose,
    ExposeProtocol, Label, Run, RunCommand, RunMount, RunMountKind, Shell, Stopsignal, User,
    Volume, Workdir,
};
pub use span::{Span, Spanned};
pub use tree::{Ast, FromArg, FromSource, Stage};
