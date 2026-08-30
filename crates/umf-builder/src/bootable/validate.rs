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
///
/// **Last wins** on a repeated label. The container path writes labels into a
/// map, so a re-declared key overwrites there; this used to return the first
/// match, which meant the same recipe resolved differently depending on what
/// `FROM` had resolved to. Re-declaring a label to override an inherited one
/// is also the Docker-shaped idiom the project deliberately follows.
pub(super) fn pick_flavor(stage: &Stage) -> (&str, bool) {
    stage
        .directives
        .iter()
        .rev()
        .find_map(|directive| match directive {
            Directive::Label(l) if l.key.value.as_str() == label::FLAVOR => {
                Some((l.value.value.as_str(), false))
            }
            _ => None,
        })
        .unwrap_or(("systemd-boot", true))
}

/// Pull the `ENTRYPOINT <init>` directive out of the stage.
///
/// Traverses in reverse for symmetry with [`pick_flavor`], but the order is
/// unobservable: the parser rejects a second `ENTRYPOINT` in a stage, so at
/// most one can ever be present. Kept consistent so a future relaxation of
/// that rule does not silently reintroduce first-wins here.
pub(super) fn pick_entrypoint(stage: &Stage) -> Option<&EntrypointInit> {
    stage
        .directives
        .iter()
        .rev()
        .find_map(|directive| match directive {
            Directive::Entrypoint(e) => Some(&e.init),
            _ => None,
        })
}

#[cfg(test)]
mod tests;
