//! AST validation + directive extraction for the bootable target.
//!
//! Pulls the **final** build [`Stage`] out of the AST — the bootable stage,
//! whose `FROM` resolves to a kernel artifact (earlier stages are container
//! producers, built separately) — enforces the bootable-build preconditions
//! (`FROM` is a reference, the builder confirms it resolves to a kernel via L0
//! introspection), and extracts the directives [`build_vm`](super::build_vm)
//! consumes (the `flavor` LABEL and ENTRYPOINT).

use umf_core::ast::{Ast, Directive, EntrypointInit, FromSource, Stage};
use umf_core::label;

use super::BootableBuildError;

/// Return the **final** stage of the AST — the bootable stage. In a multi-stage
/// recipe the earlier stages are container producers (built separately, via the
/// engine), and the last stage's `FROM` is what decides the build is bootable;
/// in a single-stage recipe the last stage is the only stage. The kernel-ness
/// of `FROM` is confirmed later via L0 introspection; here we only reject the
/// `FROM scratch` case, which has no kernel source.
pub(super) fn validate_ast_for_vm(ast: &Ast) -> Result<&Stage, BootableBuildError> {
    let stage = ast.stages.last().ok_or(BootableBuildError::EmptyAst)?;
    match &stage.from.source {
        FromSource::Reference(_) => {}
        FromSource::Scratch => return Err(BootableBuildError::VmRequiresKernelFromRef),
    }
    // CMD / VOLUME / STOPSIGNAL map to OCI container-config fields; a bootable
    // build (init system or appliance) has no use for them, so reject early.
    for directive in &stage.directives {
        let name = match directive {
            Directive::Cmd(_) => "CMD",
            Directive::Volume(_) => "VOLUME",
            Directive::Stopsignal(_) => "STOPSIGNAL",
            _ => continue,
        };
        return Err(BootableBuildError::ContainerOnlyDirective { directive: name });
    }
    Ok(stage)
}

/// Pull the boot-packaging flavor from a `LABEL org.imagilux.umf.flavor`
/// directive on the stage.
///
/// Returns the flavor value plus whether it was defaulted (label absent), so
/// the caller can warn. Default: `systemd-boot` (classic) — the common case;
/// `umf compile` validates the value and fails on an unrecognised one.
pub(super) fn pick_flavor(stage: &Stage) -> (&str, bool) {
    for directive in &stage.directives {
        if let Directive::Label(l) = directive
            && l.key.value.as_str() == label::FLAVOR
        {
            return (l.value.value.as_str(), false);
        }
    }
    ("systemd-boot", true)
}

/// Pull the `ENTRYPOINT <init>` directive out of the stage.
pub(super) fn pick_entrypoint(stage: &Stage) -> Option<&EntrypointInit> {
    for directive in &stage.directives {
        if let Directive::Entrypoint(e) = directive {
            return Some(&e.init);
        }
    }
    None
}
