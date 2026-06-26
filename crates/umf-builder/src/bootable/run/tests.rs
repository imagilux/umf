//! Unit tests for the bootable RUN command wrapping.

use super::*;

fn sh() -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string()]
}

fn bash() -> Vec<String> {
    vec!["/bin/bash".to_string(), "-c".to_string()]
}

/// The boot-critical invariant: default SHELL + no USER + no WORKDIR returns
/// the command byte-for-byte. The conventional bootable build relies on this —
/// the micro-VM guest command must be identical to the pre-wiring behaviour.
#[test]
fn default_context_returns_command_verbatim() {
    let cmd = "apk add --no-cache busybox && echo done";
    assert_eq!(wrap_run_command(cmd, &sh(), None, None), cmd);
}

/// `SHELL none` collapses to the default interpreter, so it is also a no-op
/// when USER / WORKDIR are unset.
#[test]
fn shell_none_is_treated_as_default() {
    // `SHELL none` resolves to an empty argv; the bootable walk collapses that
    // to the default interpreter, so it is a no-op when USER / WORKDIR are unset.
    assert!(is_default_shell(&resolve_shell(&[])));
    let cmd = "true";
    assert_eq!(wrap_run_command(cmd, &resolve_shell(&[]), None, None), cmd);
}

#[test]
fn workdir_prepends_cd() {
    assert_eq!(
        wrap_run_command("make", &sh(), None, Some("/src")),
        "cd '/src' && make"
    );
}

#[test]
fn custom_shell_wraps_in_interpreter() {
    // Default-shell short-circuit is bypassed; the snippet runs under bash.
    assert_eq!(
        wrap_run_command("echo $0", &bash(), None, None),
        "'/bin/bash' '-c' 'echo $0'"
    );
}

#[test]
fn user_drops_with_runuser_and_explicit_interpreter() {
    // USER forces an explicit interpreter even for the default shell, because
    // runuser needs a concrete command to exec (not a bare snippet).
    assert_eq!(
        wrap_run_command("id -un", &sh(), Some("app"), None),
        "runuser -u 'app' -- '/bin/sh' '-c' 'id -un'"
    );
}

#[test]
fn all_three_compose_user_outermost() {
    // WORKDIR (cd) innermost, SHELL the interpreter, USER outermost.
    assert_eq!(
        wrap_run_command("cargo build", &bash(), Some("builder"), Some("/work")),
        "runuser -u 'builder' -- '/bin/bash' '-c' 'cd '\\''/work'\\'' && cargo build'"
    );
}

#[test]
fn shell_quote_escapes_embedded_single_quotes() {
    assert_eq!(shell_quote("a'b"), r"'a'\''b'");
}

fn sp(s: &str) -> umf_core::ast::Spanned<String> {
    umf_core::ast::Spanned::new(s.to_string(), umf_core::ast::Span::new(0, 0))
}

fn scope(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn render_run_shell_substitutes_against_scope() {
    // Shell form: `${VAR}` expands; an unknown name stays verbatim so a guest
    // shell variable (`$HOME`) survives.
    let s = scope(&[("PKG", "busybox")]);
    assert_eq!(
        render_run_command(&RunCommand::Shell(sp("apk add ${PKG} && echo $HOME")), &s),
        "apk add busybox && echo $HOME"
    );
}

#[test]
fn render_run_exec_substitutes_then_quotes_each_arg() {
    // Exec form: each argv member is substituted, then shell-quoted, so a value
    // with spaces stays one argument.
    let s = scope(&[("BIN", "my tool")]);
    assert_eq!(
        render_run_command(
            &RunCommand::Exec(vec![sp("/usr/bin/${BIN}"), sp("--run")]),
            &s
        ),
        "'/usr/bin/my tool' '--run'"
    );
}

#[test]
fn render_run_unknown_var_is_left_verbatim() {
    let s = scope(&[]);
    assert_eq!(
        render_run_command(&RunCommand::Shell(sp("echo ${MISSING}")), &s),
        "echo ${MISSING}"
    );
}

#[test]
fn resolve_shell_uses_argv_verbatim_and_defaults_when_empty() {
    // Exec form: argv preserved verbatim (e.g. strict-mode bash).
    assert_eq!(
        resolve_shell(&[sp("/bin/bash"), sp("-euo"), sp("pipefail"), sp("-c")]),
        vec!["/bin/bash", "-euo", "pipefail", "-c"]
    );
    // Empty (`SHELL none`) → default interpreter.
    assert_eq!(resolve_shell(&[]), vec!["/bin/sh", "-c"]);
}
