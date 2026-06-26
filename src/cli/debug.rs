//! `umf debug build` — interactive directive-by-directive container
//! build debugger. Pauses before each step and offers a small REPL
//! (continue / step / inspect / breakpoint / quit).

use std::path::Path;

use thiserror::Error;
use umf_oci::registry::ImageLayout;

use crate::cli::util;

#[derive(Debug, Error)]
pub(crate) enum CliDebugError {
    #[error(transparent)]
    Recipe(#[from] util::RecipeResolveError),
    #[error("read recipe {path}: {err}")]
    ReadFile { path: String, err: std::io::Error },
    #[error("parse error in {0}")]
    Parse(String),
    #[error("invalid --break-on `{spec}`: {reason}")]
    BadBreakOn { spec: String, reason: String },
    #[error("registry: {0}")]
    Registry(#[from] umf_oci::registry::RegistryError),
    #[error("build: {0}")]
    EngineBuild(#[from] umf_builder::engine_build::EngineBuildError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Bundled `umf debug build` flags.
pub(crate) struct DebugBuildArgs<'a> {
    pub(crate) path: Option<&'a Path>,
    pub(crate) file: Option<&'a Path>,
    pub(crate) tag: &'a str,
    pub(crate) compression: umf_oci::image::LayerCompression,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) break_on: Option<&'a str>,
}

/// Parse the recipe, install the REPL hook, and drive the build —
/// pausing before each directive per the hook's breakpoint state.
pub(crate) fn run_debug_build(args: DebugBuildArgs<'_>) -> Result<(), CliDebugError> {
    let resolved = util::resolve_recipe(args.path, args.file)?;
    let source =
        std::fs::read_to_string(&resolved.recipe).map_err(|err| CliDebugError::ReadFile {
            path: resolved.recipe.display().to_string(),
            err,
        })?;
    let source_name = resolved.recipe.display().to_string();
    let ast = match umf_parser::parse_with_warnings(&source) {
        Ok((ast, warnings)) => {
            if !warnings.is_empty() {
                let mut stderr = std::io::stderr().lock();
                let _ = umf_parser::diagnostics::report_all(
                    &warnings,
                    &mut stderr,
                    &source_name,
                    &source,
                );
            }
            ast
        }
        Err(diags) => {
            let mut stderr = std::io::stderr().lock();
            let _ = umf_parser::diagnostics::report_all(&diags, &mut stderr, &source_name, &source);
            return Err(CliDebugError::Parse(source_name));
        }
    };

    // Per-debug layout dir — a tempdir keeps production caches
    // pristine. Operators who want to inspect the staging tree pass
    // an explicit --layout-dir.
    let (layout_dir, _layout_guard) = match args.layout_dir_override {
        Some(p) => (p.to_path_buf(), None),
        None => {
            let td = tempfile::tempdir()?;
            (td.path().to_path_buf(), Some(td))
        }
    };
    let layout = ImageLayout::init(&layout_dir)?;

    let context_dir = resolved.context.as_path();

    let initial_breakpoints = parse_break_on(args.break_on)?;

    let hook = std::sync::Arc::new(ReplHook::new(initial_breakpoints));
    let engine_options = umf_builder::engine_build::EngineBuildOptions {
        hook: Some(hook.clone() as umf_engine::SharedHook),
        compression: args.compression,
        ..umf_builder::engine_build::EngineBuildOptions::default()
    };

    let rt = tokio::runtime::Runtime::new()?;
    eprintln!(
        "umf debug build: {}\n  layout: {}\n  (commands: [c]ontinue / [s]tep / [i]nspect / [b]reakpoint / [q]uit)\n",
        resolved.recipe.display(),
        layout_dir.display(),
    );

    let result = rt.block_on(umf_builder::engine_build::build(
        &layout,
        context_dir,
        &ast,
        args.tag,
        &engine_options,
    ));

    hook.build_finished();

    match result {
        Ok(entry) => {
            eprintln!(
                "\numf debug build: completed.\n  manifest: {}\n  tag: {}",
                entry.digest, args.tag,
            );
            Ok(())
        }
        Err(umf_builder::engine_build::EngineBuildError::Engine(
            umf_engine::EngineError::BuildAborted {
                stage_index,
                step_index,
            },
        )) => {
            eprintln!("\numf debug build: aborted at stage {stage_index}, step {step_index}.",);
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn parse_break_on(spec: Option<&str>) -> Result<std::collections::BTreeSet<u32>, CliDebugError> {
    let Some(spec) = spec else {
        return Ok(std::collections::BTreeSet::new());
    };
    let mut out = std::collections::BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let n: u32 = part.parse().map_err(|_| CliDebugError::BadBreakOn {
            spec: spec.to_string(),
            reason: format!("{part:?} is not a step index"),
        })?;
        if n == 0 {
            return Err(CliDebugError::BadBreakOn {
                spec: spec.to_string(),
                reason: "step indices are 1-based; 0 is not valid".into(),
            });
        }
        out.insert(n);
    }
    Ok(out)
}

/// Interactive REPL hook installed by `umf debug build`. State is
/// behind a Mutex so the (sync) BuildHook callbacks can read/write
/// without unsafe.
#[derive(Debug)]
struct ReplHook {
    state: std::sync::Mutex<ReplState>,
}

#[derive(Debug)]
struct ReplState {
    breakpoints: std::collections::BTreeSet<u32>,
    /// `true` ⇒ the next `before_step` will halt the REPL.
    pause_next: bool,
}

impl ReplHook {
    fn new(breakpoints: std::collections::BTreeSet<u32>) -> Self {
        Self {
            state: std::sync::Mutex::new(ReplState {
                breakpoints,
                pause_next: true, // halt before the first step
            }),
        }
    }

    fn build_finished(&self) {
        eprintln!("\numf debug build: session ended.");
    }
}

impl umf_engine::BuildHook for ReplHook {
    fn before_step(&self, info: &umf_engine::StepInfo) -> umf_engine::HookAction {
        // Quick read: do we even pause here?
        {
            let s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let breakpoint_hit = s.breakpoints.contains(&info.step_index);
            if !s.pause_next && !breakpoint_hit {
                return umf_engine::HookAction::Continue;
            }
        }
        eprintln!(
            "\n[stage {}/{} step {}/{}] {}",
            info.stage_index, info.stage_total, info.step_index, info.step_total, info.description,
        );
        loop {
            eprint!("(umf-debug) > ");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                eprintln!();
                return umf_engine::HookAction::Abort;
            }
            let cmd = line.trim();
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match cmd {
                "" | "c" | "continue" => {
                    // Run until next breakpoint (or end if none).
                    s.pause_next = false;
                    return umf_engine::HookAction::Continue;
                }
                "s" | "step" => {
                    // Pause again before the next step.
                    s.pause_next = true;
                    return umf_engine::HookAction::Continue;
                }
                "i" | "inspect" => {
                    eprintln!(
                        "  stage {}/{}, step {}/{}, kind: {:?}",
                        info.stage_index,
                        info.stage_total,
                        info.step_index,
                        info.step_total,
                        info.kind,
                    );
                    eprintln!("  directive: {}", info.description);
                    if s.breakpoints.is_empty() {
                        eprintln!("  breakpoints: (none)");
                    } else {
                        eprintln!(
                            "  breakpoints: {}",
                            s.breakpoints
                                .iter()
                                .map(u32::to_string)
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                    // Loop back to the prompt without advancing the build.
                }
                "q" | "quit" => {
                    return umf_engine::HookAction::Abort;
                }
                _ if cmd.starts_with("b ") || cmd.starts_with("breakpoint ") => {
                    let rest = cmd.split_whitespace().nth(1).unwrap_or("");
                    if rest == "list" {
                        if s.breakpoints.is_empty() {
                            eprintln!("  no breakpoints");
                        } else {
                            for bp in &s.breakpoints {
                                eprintln!("  - {bp}");
                            }
                        }
                    } else if rest == "clear" {
                        s.breakpoints.clear();
                        eprintln!("  breakpoints cleared");
                    } else if let Ok(n) = rest.parse::<u32>() {
                        s.breakpoints.insert(n);
                        eprintln!("  breakpoint set: step {n}");
                    } else {
                        eprintln!("  unknown breakpoint command: {rest:?}");
                    }
                }
                _ => {
                    eprintln!(
                        "  unknown command {cmd:?}. valid: c|continue, s|step, i|inspect, b <N>|b list|b clear, q|quit",
                    );
                }
            }
        }
    }

    fn after_step(&self, _info: &umf_engine::StepInfo) {}
}
