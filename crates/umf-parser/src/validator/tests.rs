//! Unit tests for the `validator` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

fn errors(source: &str) -> Vec<Diagnostic> {
    let ast = match crate::grammar::parse(source, crate::lexer::tokenize(source).0) {
        Ok((ast, _warnings)) => ast,
        Err(diags) => {
            panic!("syntactic parse should succeed for validator tests; got: {diags:?}")
        }
    };
    validate(&ast)
}

fn ok(source: &str) {
    let diags = errors(source);
    assert!(
        diags.is_empty(),
        "expected no validation errors but got: {diags:?}"
    );
}

fn err_contains(source: &str, substring: &str) {
    let diags = errors(source);
    assert!(
        diags.iter().any(|d| d.message.contains(substring)),
        "expected error containing `{substring}`, got: {diags:?}"
    );
}

#[test]
fn empty_scratch_stage_validates_clean() {
    ok("FROM scratch\n");
}

#[test]
fn simple_container_validates_clean() {
    ok("FROM alpine:3.21\nENTRYPOINT none\nRUN apk add nginx\n");
}

#[test]
fn full_bootable_validates_clean() {
    ok("\
FROM imagilux/kernel-linux:7.0
LABEL org.imagilux.umf.flavor=systemd-boot
ADD debian:bookworm /
ENTRYPOINT systemd
");
}

#[test]
fn minimal_appliance_validates_clean() {
    ok("\
FROM imagilux/kernel-linux:7.0
LABEL org.imagilux.umf.flavor=uki
ENTRYPOINT /myapp
ADD myapp /myapp
");
}

#[test]
fn duplicate_entrypoint_errors() {
    err_contains(
        "FROM scratch\nENTRYPOINT systemd\nENTRYPOINT openrc\n",
        "duplicate ENTRYPOINT",
    );
}

#[test]
fn multiple_labels_allowed() {
    ok("FROM scratch\nLABEL a=1\nLABEL b=2\nLABEL c=3\n");
}

#[test]
fn multiple_runs_allowed() {
    ok("FROM alpine:3.21\nRUN echo a\nRUN echo b\nRUN echo c\n");
}

#[test]
fn add_from_image_validates_clean() {
    ok("FROM imagilux/kernel-linux:7.0\nADD debian:bookworm /\nENTRYPOINT systemd\n");
}

#[test]
fn appliance_with_rootfs_validates_clean() {
    // An appliance (binary ENTRYPOINT) runs as PID 1 on top of a rootfs
    // pulled with `ADD --from=<image>`; both are valid together.
    ok("FROM imagilux/kernel-linux:7.0\nADD debian:bookworm /\nENTRYPOINT /myapp\n");
}

#[test]
fn appliance_exec_form_with_rootfs_validates_clean() {
    ok(
        "FROM imagilux/kernel-linux:7.0\nADD debian:bookworm /\nENTRYPOINT [\"/myapp\", \"--flag\"]\n",
    );
}

#[test]
fn entrypoint_path_in_container_is_allowed() {
    ok("FROM alpine:3.21\nENTRYPOINT /usr/sbin/nginx\n");
}

#[test]
fn entrypoint_exec_form_in_container_is_allowed() {
    ok("FROM alpine:3.21\nENTRYPOINT [\"/usr/sbin/nginx\", \"-g\", \"daemon off;\"]\n");
}

#[test]
fn duplicate_stage_name_errors() {
    err_contains(
        "FROM debian:bookworm AS builder\nRUN make\n\nFROM alpine:3.21 AS builder\nRUN ls\n",
        "duplicate stage name",
    );
}

#[test]
fn distinct_stage_names_validate_clean() {
    ok("\
FROM debian:bookworm AS builder
RUN make

FROM alpine:3.21 AS runtime
RUN ls
");
}
