//! Unit tests for the `hooks` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::sync::Mutex;

/// Programmatic hook that records every callback invocation.
/// Used in the umf-builder tests + as the pattern the umf debug
/// REPL follows.
#[derive(Debug)]
struct RecordingHook(Mutex<Vec<String>>);

impl BuildHook for RecordingHook {
    fn before_step(&self, info: &StepInfo) -> HookAction {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("before {} {:?}", info.step_index, info.kind));
        HookAction::Continue
    }
    fn after_step(&self, info: &StepInfo) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("after {} {:?}", info.step_index, info.kind));
    }
    fn build_finished(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("build_finished".to_string());
    }
}

#[test]
fn noop_hook_returns_continue_and_does_nothing() {
    let h = NoopHook;
    let info = StepInfo {
        stage_index: 1,
        stage_total: 1,
        step_index: 1,
        step_total: 3,
        kind: StepKind::Run,
        description: "RUN echo hi".to_string(),
    };
    assert_eq!(h.before_step(&info), HookAction::Continue);
    h.after_step(&info);
    h.build_finished();
}

#[test]
fn recording_hook_captures_lifecycle() {
    let h = RecordingHook(Mutex::new(Vec::new()));
    let info = StepInfo {
        stage_index: 1,
        stage_total: 1,
        step_index: 1,
        step_total: 1,
        kind: StepKind::Add,
        description: "ADD foo /dst".to_string(),
    };
    let _ = h.before_step(&info);
    h.after_step(&info);
    h.build_finished();
    let log = h.0.into_inner().unwrap();
    assert_eq!(log, vec!["before 1 Add", "after 1 Add", "build_finished"]);
}

#[test]
fn shared_hook_round_trips_via_arc() {
    let shared = noop_shared();
    let info = StepInfo {
        stage_index: 1,
        stage_total: 1,
        step_index: 1,
        step_total: 1,
        kind: StepKind::Metadata,
        description: "LABEL foo=bar".to_string(),
    };
    assert_eq!(shared.before_step(&info), HookAction::Continue);
}
