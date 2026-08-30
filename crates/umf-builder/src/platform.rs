//! Selecting the right manifest out of a multi-arch image index.
//!
//! An OCI image reference can resolve to an *index* listing one manifest per
//! platform. Anything that needs to know what an image actually is — its
//! layers, its config, its `org.imagilux.umf.type` — must first pick the
//! child matching the build's architecture. Doing that in one place keeps the
//! base-image resolver and the L0 introspector from disagreeing about which
//! child a reference denotes.

use oci_client::manifest::{ImageIndexEntry, OciImageIndex};
use oci_spec::image::{Arch, Os};
use umf_core::architecture::Architecture;

/// Pick the manifest for `architecture` from `index`, or `None` when the
/// index lists no Linux manifest for it.
///
/// Prefers an exact variant-less match, then any variant of the same
/// architecture — so `arm64` picks the plain entry over `arm64/v8` when both
/// are present, but still resolves when only a variant-qualified entry
/// exists. Never falls back to a different architecture: silently returning
/// the wrong one would produce an image whose layers do not match its
/// declared platform.
#[must_use]
pub(crate) fn select_for_arch(
    index: &OciImageIndex,
    architecture: Architecture,
) -> Option<&ImageIndexEntry> {
    let want = Arch::from(architecture.oci_arch_string());
    index
        .manifests
        .iter()
        .find(|m| {
            m.platform.as_ref().is_some_and(|p| {
                p.os == Os::Linux
                    && p.architecture == want
                    && p.variant.as_deref().is_none_or(str::is_empty)
            })
        })
        .or_else(|| {
            index.manifests.iter().find(|m| {
                m.platform
                    .as_ref()
                    .is_some_and(|p| p.os == Os::Linux && p.architecture == want)
            })
        })
}

#[cfg(test)]
mod tests;
