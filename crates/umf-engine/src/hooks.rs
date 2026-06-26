//! Build-step lifecycle hooks.
//!
//! The [`BuildHook`] trait lets a caller observe and steer the
//! container-build pipeline at directive granularity. Today's
//! consumer is `umf debug build`'s interactive REPL; tests can
//! install programmatic hooks for deterministic verification.
//!
//! ## Lifecycle
//!
//! For each directive in each stage:
//!
//! ```text
//!   hook.before_step(&StepInfo) -> HookAction
//!     - Continue: execute the directive
//!     - Abort:    return cleanly with EngineError::BuildAborted
//!   apply_directive(...)
//!   hook.after_step(&StepInfo)
//! ```
//!
//! Metadata-only directives (`LABEL`, `ENV`, `ARG`, …) still fire
//! their callbacks — operators stepping through a build often want
//! to see those land on the in-progress image config.

use std::sync::Arc;

/// Coarse kind of a build step — what the directive is doing,
/// without committing to a specific directive shape (the engine
/// doesn't want hooks to depend on the AST module layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// `RUN <cmd>` — produces a layer from a command execution.
    Run,
    /// `ADD <src> <dst>` / `ADD --from=<stage> ...` — produces a
    /// layer from a copy operation.
    Add,
    /// `LABEL`, `ENV`, `ARG`, `ENTRYPOINT`, `WORKDIR`, ... —
    /// metadata-only directives that don't produce filesystem layers
    /// but mutate the in-progress image config.
    Metadata,
}

/// Per-step information passed to [`BuildHook`] callbacks.
#[derive(Debug, Clone)]
pub struct StepInfo {
    /// 1-based stage index in the recipe.
    pub stage_index: usize,
    /// Total stages in the recipe.
    pub stage_total: usize,
    /// 1-based step index within the current stage.
    pub step_index: u32,
    /// Total directives in the current stage.
    pub step_total: u32,
    /// Kind of step (see [`StepKind`]).
    pub kind: StepKind,
    /// Short human description of the directive (e.g.
    /// `RUN apt-get install nginx`).
    pub description: String,
}

/// What [`BuildHook::before_step`] tells the engine to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Execute the directive normally.
    Continue,
    /// Abort the build cleanly — the engine returns a
    /// [`crate::EngineError::BuildAborted`] without executing the
    /// pending directive or any subsequent ones.
    Abort,
}

/// Hook the engine calls at each build-step boundary.
///
/// Default no-op impl is provided via [`NoopHook`]. Real-world
/// implementors:
///
/// - The interactive REPL behind `umf debug build`.
/// - Tests that install a programmatic hook to assert on the
///   recipe's execution shape (step count, kinds, descriptions).
pub trait BuildHook: Send + Sync + std::fmt::Debug {
    /// Called before each directive executes. Returning
    /// [`HookAction::Abort`] stops the build before any side effects
    /// of the pending directive land.
    fn before_step(&self, info: &StepInfo) -> HookAction;

    /// Called after each directive executes (whether it produced a
    /// layer or just mutated config). Errors from the directive are
    /// surfaced via the engine's return value, not through the hook;
    /// `after_step` only fires on success.
    fn after_step(&self, info: &StepInfo);

    /// Called when the whole build finishes (success or after an
    /// abort). Lets the hook flush state, close TTYs, print a final
    /// "session ended" message, etc. Default impl is a no-op.
    fn build_finished(&self) {}
}

/// No-op [`BuildHook`] — every callback is a no-op, every action is
/// `Continue`. This is what `engine_build::build` installs when the
/// caller doesn't pass an explicit hook.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHook;

impl BuildHook for NoopHook {
    fn before_step(&self, _info: &StepInfo) -> HookAction {
        HookAction::Continue
    }
    fn after_step(&self, _info: &StepInfo) {}
}

/// Shared-handle alias — what [`crate::backends`] and `umf-builder`
/// thread through. `Arc` keeps the hook cheap to clone across
/// stage-builder closures + libcontainer handlers.
pub type SharedHook = Arc<dyn BuildHook>;

/// Construct the default `NoopHook` wrapped in [`SharedHook`].
#[must_use]
pub fn noop_shared() -> SharedHook {
    Arc::new(NoopHook)
}

#[cfg(test)]
mod tests;
