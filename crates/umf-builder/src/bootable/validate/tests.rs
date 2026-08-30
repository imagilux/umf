//! Unit tests for the `validate` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use umf_parser::parse;

fn last_stage(src: &str) -> Stage {
    let ast = parse(src).expect("parse");
    ast.stages.into_iter().next_back().expect("a stage")
}

#[test]
fn a_repeated_flavor_label_takes_the_last_value() {
    // Re-declaring a label to override an earlier one is the Docker-shaped
    // idiom, and the container path already behaves this way (labels go into
    // a map). First-wins here meant the same recipe resolved differently
    // depending on what FROM had resolved to.
    let stage = last_stage(
        "FROM registry.example.com/kernels/linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         LABEL org.imagilux.umf.flavor=uki\n\
         ENTRYPOINT systemd\n",
    );
    assert_eq!(pick_flavor(&stage), ("uki", false));
}

#[test]
fn a_single_flavor_label_is_used_and_absence_defaults() {
    let declared = last_stage(
        "FROM registry.example.com/kernels/linux:7.0\n\
         LABEL org.imagilux.umf.flavor=uki\n\
         ENTRYPOINT systemd\n",
    );
    assert_eq!(pick_flavor(&declared), ("uki", false));

    let absent = last_stage("FROM registry.example.com/kernels/linux:7.0\nENTRYPOINT systemd\n");
    let (flavor, defaulted) = pick_flavor(&absent);
    assert_eq!(flavor, "systemd-boot");
    assert!(defaulted, "an absent label must report as defaulted");
}

#[test]
fn unrelated_labels_do_not_affect_the_flavor() {
    let stage = last_stage(
        "FROM registry.example.com/kernels/linux:7.0\n\
         LABEL org.opencontainers.image.version=1.2.3\n\
         LABEL org.imagilux.umf.flavor=uki\n\
         LABEL maintainer=someone\n\
         ENTRYPOINT systemd\n",
    );
    assert_eq!(pick_flavor(&stage), ("uki", false));
}

#[test]
fn a_duplicate_entrypoint_is_rejected_by_the_parser() {
    // Unlike LABEL, ENTRYPOINT cannot repeat: the parser refuses a second one
    // outright, so `pick_entrypoint`'s traversal order is unobservable for any
    // recipe that parses. Pinned here so the guarantee `pick_entrypoint`
    // relies on stays a guarantee.
    let src = "FROM registry.example.com/kernels/linux:7.0\n\
               ENTRYPOINT openrc\n\
               ENTRYPOINT systemd\n";
    let diagnostics = parse(src).expect_err("a duplicate ENTRYPOINT must not parse");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("duplicate ENTRYPOINT")),
        "expected a duplicate-ENTRYPOINT diagnostic, got {diagnostics:?}",
    );
}

#[test]
fn no_entrypoint_yields_none() {
    let stage = last_stage("FROM registry.example.com/kernels/linux:7.0\n");
    assert!(pick_entrypoint(&stage).is_none());
}
