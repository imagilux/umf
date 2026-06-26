//! Unit tests for the `render` module.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::path::PathBuf;
use umf_parser::parse;

fn render(src: &str) -> String {
    let ast = parse(src).expect("parse");
    render_ast(&ast, &PathBuf::from("test.umf"))
}

#[test]
fn minimal_scratch_renders_with_one_stage() {
    let out = render("FROM scratch\n");
    assert!(out.contains("File: test.umf"));
    assert!(out.contains("1 stage"));
    assert!(out.contains("Stage 1"));
    assert!(out.contains("FROM"));
    assert!(out.contains("scratch"));
}

#[test]
fn bootable_build_renders_directives() {
    let out = render(
        "FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         ADD debian:bookworm /\n\
         ENTRYPOINT systemd\n",
    );
    assert!(out.contains("imagilux/kernel-linux:7.0"));
    assert!(out.contains("org.imagilux.umf.flavor"));
    assert!(out.contains("debian:bookworm"));
    assert!(out.contains("ENTRYPOINT"));
    assert!(out.contains("systemd"));
}

#[test]
fn entrypoint_path_renders_verbatim() {
    let out = render(
        "FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=uki\n\
         ENTRYPOINT /myapp --flag value\n",
    );
    assert!(out.contains("/myapp --flag value"));
}

#[test]
fn entrypoint_exec_form_renders_as_argv_list() {
    let out =
        render("FROM alpine:3.21\nENTRYPOINT [\"/usr/sbin/nginx\", \"-g\", \"daemon off;\"]\n");
    assert!(out.contains("[\"/usr/sbin/nginx\", \"-g\", \"daemon off;\"]"));
}

#[test]
fn add_with_from_shows_origin_stage() {
    let out = render(
        "FROM debian:bookworm AS builder\nRUN echo hi\n\nFROM alpine:3.21\nADD --from=builder /app /app\n",
    );
    assert!(out.contains("AS builder"));
    assert!(out.contains("/app → /app  (--from=builder)"));
}

#[test]
fn multi_stage_renders_two_stage_headings() {
    let out = render(
        "FROM debian:bookworm AS builder\nRUN echo hi\n\nFROM alpine:3.21\nADD alpine:3.21 /\n",
    );
    assert!(out.contains("Stage 1 (AS builder)"));
    assert!(out.contains("Stage 2"));
    assert!(out.contains("2 stages"));
}

#[test]
fn long_run_command_truncated_with_ellipsis() {
    let long = "echo ".repeat(40); // far past MAX_VALUE_WIDTH
    let src = format!("FROM alpine:3.21\nRUN {long}\n");
    let out = render(&src);
    // Ellipsis appears, full repetition does not.
    assert!(out.contains('…'));
    assert!(!out.contains(&long.trim_end().to_string()));
}

#[test]
fn label_renders_key_equals_value() {
    let out = render("FROM scratch\nLABEL author=Test\n");
    assert!(out.contains("LABEL"));
    assert!(out.contains("author = Test"));
}

#[test]
fn expose_renders_port_slash_proto() {
    let out = render("FROM alpine:3.21\nEXPOSE 443/tcp\nEXPOSE 53/udp\n");
    assert!(out.contains("443/tcp"));
    assert!(out.contains("53/udp"));
}
