//! VM RUN-step orchestration.
//!
//! Walks a stage's directive list in source order, executing each `RUN` in a
//! micro-VM ([`run_step_vm`]) whose root is the build staging tree. `ENV`
//! directives encountered along the way accumulate into the environment that
//! subsequent RUN steps observe; `SHELL` / `USER` / `WORKDIR` track the same
//! way and are folded into each RUN command host-side ([`wrap_run_command`]).
//! The micro-VM guest init is never touched, so a recipe that sets none of
//! them produces a byte-identical guest command — the boot-smoke / privileged
//! lanes depend on that.

use std::collections::BTreeMap;

use umf_core::ast::{Directive, RunCommand, Spanned, Stage};

use crate::arg_subst::{apply_arg_to_scope, subst_with};
use crate::kernel::KernelLayout;
use crate::vm_runner::{RunStepConfig, RunStepResult, run_step_vm};
use umf_oci::staging::BuildStaging;

use super::{BootableBuildError, BootableBuildOptions};

/// Walk the directive list in source order, picking out every `RUN`
/// directive and executing it in the micro-VM. ENV / SHELL / USER / WORKDIR
/// directives encountered along the way accumulate into the context
/// subsequent RUN steps see (Docker positional semantics).
pub(super) async fn run_all_run_directives(
    stage: &Stage,
    staging: &BuildStaging,
    kernel: &KernelLayout,
    options: &BootableBuildOptions,
    globals: &BTreeMap<String, String>,
) -> Result<Vec<RunStepResult>, BootableBuildError> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut reports: Vec<RunStepResult> = Vec::new();

    // Build-step context tracked in source order (Docker semantics): SHELL
    // sets the RUN interpreter, USER the executing user, WORKDIR the working
    // directory — each applies to the RUN steps that follow it. They are
    // folded into the command host-side ([`wrap_run_command`]); the micro-VM
    // guest init is unchanged.
    let mut shell = default_shell();
    let mut user: Option<String> = None;
    let mut workdir: Option<String> = None;

    // `${VAR}` / `$VAR` substitution scope, seeded from the build globals and
    // extended positionally by any in-stage `ARG`. The RUN command,
    // ENV value, USER, and WORKDIR are all substituted against it before they
    // reach the guest — unknown names stay verbatim so shell `$HOME` / `$(date)`
    // survive. Mirrors the container engine's `${VAR}` handling so a recipe
    // means the same on both targets.
    let mut arg_scope = globals.clone();

    for directive in &stage.directives {
        match directive {
            Directive::Arg(arg) => {
                apply_arg_to_scope(&mut arg_scope, arg, &options.build_args);
            }
            Directive::Env(e) => {
                env.insert(
                    e.key.value.to_string(),
                    subst_with(&arg_scope, e.value.value.as_str()),
                );
            }
            Directive::Shell(s) => {
                shell = resolve_shell(&s.argv);
            }
            Directive::User(u) => {
                user = Some(subst_with(&arg_scope, u.name.value.as_str()));
            }
            Directive::Workdir(w) => {
                // Docker-faithful, matching the container engine's `apply_workdir`:
                // resolve a relative path against the current working dir and
                // create the directory in the staging rootfs, so the RUN `cd`
                // succeeds and the directory ships in the image.
                let requested = subst_with(&arg_scope, w.path.value.as_str());
                let current = workdir.as_deref().unwrap_or("/");
                let resolved = crate::fsutil::resolve_workdir(current, &requested);
                std::fs::create_dir_all(staging.path().join(resolved.trim_start_matches('/')))?;
                workdir = Some(resolved);
            }
            Directive::Run(r) => {
                let qemu_path = options
                    .qemu_path
                    .clone()
                    .ok_or(BootableBuildError::RunWithoutQemu)?;
                let raw = render_run_command(&r.command, &arg_scope);
                let command = wrap_run_command(&raw, &shell, user.as_deref(), workdir.as_deref());
                let mut config =
                    RunStepConfig::new(qemu_path, options.kvm_available, command.clone());
                config.env = env.clone();

                let result = run_step_vm(staging, kernel, &config).await?;
                if result.exit_code != 0 {
                    return Err(BootableBuildError::RunStepFailed {
                        command: command.clone(),
                        exit_code: result.exit_code,
                        serial_output: result.serial_output.clone(),
                    });
                }
                reports.push(result);
            }
            _ => {}
        }
    }

    Ok(reports)
}

/// Render a `RUN` command to the raw shell string the guest will run, with
/// `${VAR}` / `$VAR` substituted against `arg_scope`. Shell form is
/// substituted as a whole; exec form substitutes then shell-quotes each argv
/// member (so a substituted value containing spaces stays a single argument).
/// Unknown names are left verbatim, so a guest-side `$HOME` / `$(date)` survives.
fn render_run_command(command: &RunCommand, arg_scope: &BTreeMap<String, String>) -> String {
    match command {
        RunCommand::Shell(s) => subst_with(arg_scope, &s.value),
        RunCommand::Exec(argv) => argv
            .iter()
            .map(|a| shell_quote(&subst_with(arg_scope, &a.value)))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Default RUN interpreter — Docker's `/bin/sh -c`. A recipe that never issues
/// a `SHELL` directive leaves this in force, and [`wrap_run_command`] then
/// hands the guest the command verbatim.
fn default_shell() -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string()]
}

/// Resolve a `SHELL` directive's argv for the bootable RUN walk. The parser has
/// already expanded the keyword forms and preserved the exec form verbatim; an
/// empty argv (`SHELL none`) collapses to the default, because the micro-VM
/// guest always needs a concrete interpreter for RUN (matching the engine's
/// empty-shell fallback at the use site).
fn resolve_shell(argv: &[Spanned<String>]) -> Vec<String> {
    if argv.is_empty() {
        default_shell()
    } else {
        argv.iter().map(|a| a.value.clone()).collect()
    }
}

/// True when `shell` is the conventional `/bin/sh -c`.
fn is_default_shell(shell: &[String]) -> bool {
    shell.len() == 2 && shell[0] == "/bin/sh" && shell[1] == "-c"
}

/// Fold the active SHELL / USER / WORKDIR context into a single shell command
/// string the micro-VM guest runs (the guest wraps it in its own `/bin/sh -c`).
///
/// **Default-unchanged invariant (boot-critical):** when `shell` is the default
/// `/bin/sh -c` and neither USER nor WORKDIR is set, the command is returned
/// *verbatim*. The conventional bootable build (which uses none of these
/// directives) therefore hands the guest exactly the string it did before they
/// were wired — the boot-smoke / privileged lanes assert that byte-for-byte.
///
/// Otherwise the command is wrapped, innermost-first:
/// 1. **WORKDIR** → `cd <dir> && <command>`. Each RUN is a fresh micro-VM, so
///    the directory is re-established with an absolute `cd` every step rather
///    than relying on a persistent cwd.
/// 2. **SHELL** → the snippet runs through the recipe's interpreter argv
///    (`'/bin/bash' '-c' '<snippet>'`). The interpreter is also spelled out
///    when USER is set, because `runuser` needs a concrete command to exec.
/// 3. **USER** → `runuser -u <user> -- <...>` drops to the target user
///    (util-linux `runuser`, no login shell) before running everything above.
fn wrap_run_command(
    command: &str,
    shell: &[String],
    user: Option<&str>,
    workdir: Option<&str>,
) -> String {
    let default_shell = is_default_shell(shell);
    if default_shell && user.is_none() && workdir.is_none() {
        return command.to_string();
    }

    let snippet = match workdir {
        Some(dir) => format!("cd {} && {command}", shell_quote(dir)),
        None => command.to_string(),
    };

    let interpreted = if !default_shell || user.is_some() {
        let argv = shell
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{argv} {}", shell_quote(&snippet))
    } else {
        snippet
    };

    match user {
        Some(u) => format!("runuser -u {} -- {interpreted}", shell_quote(u)),
        None => interpreted,
    }
}

/// Conservative single-quote shell escape — wrap in single quotes,
/// escape embedded single quotes by closing-and-reopening with an
/// escaped literal. Used for `RUN` exec-form → shell translation and by
/// [`wrap_run_command`].
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests;
