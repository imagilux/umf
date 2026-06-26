//! Unit tests for the `directives` module (dispatch helpers + the
//! metadata/runtime handlers). The ADD path-helper tests live in
//! `super::add`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::engine_build::state::test_support::empty_state;

#[test]
fn truncate_for_prompt_is_char_safe() {
    // 78 ASCII + a multi-byte char at the truncation boundary: the old
    // `&oneline[..max-1]` byte-slice panicked here.
    let s = format!("{}é-and-some-more-text", "a".repeat(78));
    let out = truncate_for_prompt(&s, 80);
    assert!(out.ends_with('…'));
    assert!(out.chars().count() <= 80);
    // Short strings pass through unchanged.
    assert_eq!(truncate_for_prompt("RUN echo café", 80), "RUN echo café");
}

// ── UMF-native semantics — directives that emit synthesised
//     upper-dirs without invoking libcontainer ─────────────────────

#[test]
fn expose_appends_to_image_config_exposed_ports() {
    let mut state = empty_state();
    apply_expose(
        &mut state,
        &Expose {
            port: 80,
            protocol: ExposeProtocol::Tcp,
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    apply_expose(
        &mut state,
        &Expose {
            port: 443,
            protocol: ExposeProtocol::Tcp,
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    // Duplicate suppressed.
    apply_expose(
        &mut state,
        &Expose {
            port: 80,
            protocol: ExposeProtocol::Tcp,
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    assert_eq!(
        state.image_config.container.exposed_ports,
        vec!["80/tcp".to_string(), "443/tcp".to_string()],
    );
}

// HOSTNAME / LOCALE / TIMEZONE are not UMF directives; the builder has no
// apply-paths for them. Host/locale/timezone are first-boot concerns
// (cloud-init / ignition).

fn sp(s: &str) -> umf_core::ast::Spanned<String> {
    umf_core::ast::Spanned::new(s.to_string(), umf_core::ast::Span::new(0, 0))
}

#[test]
fn cmd_exec_sets_oci_cmd() {
    let mut state = empty_state();
    apply_cmd(
        &mut state,
        &Cmd {
            command: CmdForm::Exec(vec![sp("/app"), sp("--flag")]),
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    assert_eq!(
        state.image_config.container.cmd,
        Some(vec!["/app".to_string(), "--flag".to_string()])
    );
}

#[test]
fn cmd_shell_wraps_with_the_build_shell() {
    let mut state = empty_state();
    apply_cmd(
        &mut state,
        &Cmd {
            command: CmdForm::Shell(sp("echo hi")),
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    // empty_state's current_shell defaults to `/bin/sh -c`.
    assert_eq!(
        state.image_config.container.cmd,
        Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hi".to_string()
        ])
    );
}

#[test]
fn volume_dedups_and_sets_oci_volumes() {
    let mut state = empty_state();
    apply_volume(
        &mut state,
        &Volume {
            paths: vec![sp("/data"), sp("/data"), sp("/log")],
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    assert_eq!(
        state.image_config.container.volumes,
        vec!["/data".to_string(), "/log".to_string()]
    );
}

#[test]
fn stopsignal_sets_oci_stop_signal() {
    let mut state = empty_state();
    apply_stopsignal(
        &mut state,
        &Stopsignal {
            signal: sp("SIGTERM"),
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    assert_eq!(
        state.image_config.container.stop_signal,
        Some("SIGTERM".to_string())
    );
}

// ── ARG `${VAR}` substitution ───────────────────────────────────
//
// The invariant under test: a directive's *stored* value is substituted against
// the ARG scope, while its history line keeps the original `${VAR}` text — so an
// ARG value never lands in the image history.

/// Last history line's `created_by`, for the no-leak assertions.
fn last_history(state: &BuildState) -> String {
    state
        .image_config
        .history
        .last()
        .and_then(|h| h.created_by.clone())
        .expect("a history entry")
}

fn env_directive(key: &str, value: &str) -> Env {
    use umf_core::types::{EnvVarName, EnvVarValue};
    Env {
        key: umf_core::ast::Spanned::new(
            EnvVarName::new(key).unwrap(),
            umf_core::ast::Span::new(0, 0),
        ),
        value: umf_core::ast::Spanned::new(
            EnvVarValue::new(value).unwrap(),
            umf_core::ast::Span::new(0, 0),
        ),
        span: umf_core::ast::Span::new(0, 0),
    }
}

fn label_directive(key: &str, value: &str) -> Label {
    use umf_core::types::{LabelKey, LabelValue};
    Label {
        key: umf_core::ast::Spanned::new(
            LabelKey::new(key).unwrap(),
            umf_core::ast::Span::new(0, 0),
        ),
        value: umf_core::ast::Spanned::new(
            LabelValue::new(value).unwrap(),
            umf_core::ast::Span::new(0, 0),
        ),
        span: umf_core::ast::Span::new(0, 0),
    }
}

#[test]
fn env_substitutes_value_into_config_but_history_keeps_original() {
    let mut state = empty_state();
    state
        .arg_scope
        .insert("VERSION".to_string(), "1.0".to_string());
    apply_env(&mut state, &env_directive("APP_VERSION", "v${VERSION}"));
    // Config (persisted, author-intended) carries the substituted value …
    assert!(
        state
            .image_config
            .container
            .env
            .contains(&"APP_VERSION=v1.0".to_string()),
        "env: {:?}",
        state.image_config.container.env
    );
    // … but the history keeps the original `${VERSION}` text — no leak.
    assert_eq!(last_history(&state), "ENV APP_VERSION=v${VERSION}");
}

#[test]
fn label_substitutes_value_but_history_keeps_original() {
    let mut state = empty_state();
    state
        .arg_scope
        .insert("REV".to_string(), "abc123".to_string());
    apply_label(
        &mut state,
        &label_directive("org.opencontainers.image.revision", "$REV"),
    );
    assert_eq!(
        state
            .image_config
            .container
            .labels
            .get("org.opencontainers.image.revision")
            .map(String::as_str),
        Some("abc123")
    );
    assert_eq!(
        last_history(&state),
        "LABEL org.opencontainers.image.revision=$REV"
    );
}

#[test]
fn cmd_substitutes_against_arg_scope() {
    let mut state = empty_state();
    state
        .arg_scope
        .insert("BIN".to_string(), "myapp".to_string());
    apply_cmd(
        &mut state,
        &Cmd {
            command: CmdForm::Exec(vec![sp("/usr/bin/${BIN}"), sp("--serve")]),
            span: umf_core::ast::Span::new(0, 0),
        },
    );
    assert_eq!(
        state.image_config.container.cmd,
        Some(vec!["/usr/bin/myapp".to_string(), "--serve".to_string()])
    );
    // History keeps the original placeholder.
    assert_eq!(last_history(&state), "CMD /usr/bin/${BIN} --serve");
}

#[test]
fn unknown_variable_is_left_verbatim() {
    // No ARG named FOO is in scope, so `${FOO}` passes through untouched (it is
    // not blanked) — both in the stored value and the history.
    let mut state = empty_state();
    apply_env(&mut state, &env_directive("X", "${FOO}"));
    assert!(
        state
            .image_config
            .container
            .env
            .contains(&"X=${FOO}".to_string()),
        "env: {:?}",
        state.image_config.container.env
    );
}
