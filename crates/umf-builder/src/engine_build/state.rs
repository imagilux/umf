//! Per-stage build state carried across directives.
//!
//! [`BuildState`] accumulates the in-progress image-config, the FROM
//! layer chain, the new layers produced by RUN/ADD steps (plus their
//! upper-dir drop guards), and the pending init-system unit actions.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::debug;
use umf_core::l0::L0Kind;
use umf_engine::error::EngineError;
use umf_engine::overlay::PersistedUpper;
use umf_oci::image::{HistoryEntry, ImageConfig, LayerCompression, LayerSource};
use umf_oci::registry::layout::sha256_digest;

use super::EngineBuildError;
use super::util::now_rfc3339;

/// What we know about the base image after resolving FROM.
pub(crate) struct BaseImage {
    /// Layer descriptors + per-descriptor diff_ids, in manifest order.
    pub(crate) layers: Vec<LayerSource>,
    /// Parsed image-config we'll inherit from.
    pub(crate) config: ImageConfig,
}

/// State we carry forward across directives.
pub(crate) struct BuildState {
    /// Inherited from the FROM image, mutated by directives along the way.
    pub(crate) image_config: ImageConfig,
    /// FROM layer chain (bottom of the final image).
    pub(crate) base_layers: Vec<LayerSource>,
    /// Layers we've produced (one per RUN / ADD step), in build order.
    pub(crate) new_layers: Vec<LayerSource>,
    /// Drop guards for the on-disk upper-dirs corresponding to
    /// `new_layers`. Kept alive so subsequent overlays can stack them
    /// as lowers, and so layer packaging can re-read them.
    pub(crate) upper_guards: Vec<PersistedUpper>,
    /// Default shell argv for shell-form `RUN`. Updated by `SHELL`.
    pub(crate) current_shell: Vec<String>,
    /// Set once any step has been satisfied from the cache via
    /// [`Self::adopt_cached_layer`]. An adopted layer is recorded in
    /// `new_layers` but has no `upper_guards` entry, so the overlay lower
    /// stack no longer reflects it — a subsequent `RUN` would execute against
    /// an incomplete filesystem. `build_one_stage` checks this to detect a
    /// partial-hit hazard and rebuild the stage without cache lookups.
    pub(crate) adopted_from_cache: bool,
    /// Compression codec every [`Self::push_new_layer`] packages with.
    /// Also folded into the step-cache keys (see `cache.rs`), so cached
    /// layers of one codec are never adopted by a build using another.
    pub(crate) compression: LayerCompression,
    /// In-scope `ARG` values for `${VAR}` / `$VAR` substitution.
    /// Seeded from the build-global scope (pre-`FROM` ARGs resolved against
    /// `--build-arg`); an in-stage `ARG` adds or overrides entries
    /// positionally as the directive walk proceeds. Drives the value executed
    /// / stored and the layer-cache key, never the image history.
    pub(crate) arg_scope: BTreeMap<String, String>,
}

impl BuildState {
    pub(crate) fn new(
        base: BaseImage,
        compression: LayerCompression,
        globals: &BTreeMap<String, String>,
    ) -> Self {
        let mut image_config = base.config;
        // Ensure umf_type is Container — the engine path is container-only.
        image_config.umf_type = L0Kind::Container;
        // History gets a fresh "umf-engine" leading entry below as
        // RUN/ADD/metadata operations execute, so callers can audit
        // which builder produced which layer.
        Self {
            image_config,
            base_layers: base.layers,
            new_layers: Vec::new(),
            upper_guards: Vec::new(),
            current_shell: vec!["/bin/sh".to_string(), "-c".to_string()],
            adopted_from_cache: false,
            compression,
            // Each stage starts from the build-global scope; in-stage ARGs
            // mutate this clone without leaking into sibling stages.
            arg_scope: globals.clone(),
        }
    }

    /// Expand `${VAR}` / `$VAR` references in `text` against the current `ARG`
    /// scope. Unknown names are left verbatim. Used for the value
    /// a directive *executes or stores*; the history line keeps the original.
    pub(crate) fn subst(&self, text: &str) -> String {
        umf_core::subst::substitute(text, |n| self.arg_scope.get(n).map(String::as_str))
    }

    /// Compute the list of lower directories for the next overlay.
    /// Top → bottom; previous uppers (newest first), then the base
    /// image's lower stack (`base_lowers`, already newest → oldest).
    ///
    /// `base_lowers` is what the bundle exposes for the FROM image —
    /// either the single merged rootfs directory or, under the erofs
    /// strategy, one mountpoint per base layer (see
    /// [`umf_engine::bundle::Bundle::base_lowers`]).
    pub(crate) fn lower_stack<'a>(&'a self, base_lowers: &[&'a Path]) -> Vec<&'a Path> {
        let mut lowers: Vec<&Path> = self.upper_guards.iter().rev().map(|u| u.path()).collect();
        lowers.extend_from_slice(base_lowers);
        lowers
    }

    /// Final image-config after all directives have been applied. Pure
    /// data extraction — no mutation.
    pub(crate) fn finalise_image_config(&self) -> ImageConfig {
        self.image_config.clone()
    }

    /// Assemble the full layer chain we'll write into the final image:
    /// base layers first, then our new layers in build order.
    pub(crate) fn assemble_layer_chain(&self) -> Result<Vec<LayerSource>, EngineBuildError> {
        let mut out = Vec::with_capacity(self.base_layers.len() + self.new_layers.len());
        for layer in &self.base_layers {
            out.push(layer.clone());
        }
        for layer in &self.new_layers {
            out.push(layer.clone());
        }
        Ok(out)
    }

    /// Append a freshly-produced layer (from an upper-dir we own a
    /// drop guard for) plus a `history` entry for image-config audit.
    /// Returns a borrow of the pushed layer so the caller can populate
    /// the cache entry.
    pub(crate) fn push_new_layer(
        &mut self,
        upper: PersistedUpper,
        history_line: String,
    ) -> Result<&LayerSource, EngineBuildError> {
        let source = LayerSource::from_directory_with(upper.path(), self.compression)?;
        debug!(
            diff_id = %source.diff_id,
            blob = %sha256_digest(&source.data),
            "engine build: packaged new layer",
        );
        self.new_layers.push(source);
        self.upper_guards.push(upper);
        self.image_config.history.push(HistoryEntry {
            created: Some(now_rfc3339()),
            created_by: Some(history_line),
            author: Some("umf-engine".to_string()),
            comment: None,
            empty_layer: false,
        });
        // SAFETY: we just pushed `source` above; `last()` is `Some(&source)`.
        // Clippy doesn't track this; the `ok_or_else` is unreachable in
        // practice.
        self.new_layers.last().ok_or_else(|| {
            EngineError::runtime("BUG: just-pushed layer disappeared from BuildState", None).into()
        })
    }

    /// Reuse a cached layer: take ownership of the LayerSource we
    /// already produced on a previous build, plus its history entry.
    /// Adds it to `new_layers` and records the history without
    /// populating `upper_guards` — no upper-dir exists for a cache
    /// hit, and subsequent overlays don't need it as a lower because
    /// the lower-layer data is already present in the cumulative
    /// layout layer chain.
    ///
    /// Dropping `upper_guards` means subsequent RUN steps within the same
    /// build won't see this layer's contents in their overlay lower stack —
    /// only the *base* image's. For a no-change rebuild that's fine (every
    /// subsequent step is a cache hit too, so none mounts an overlay). The
    /// partial-hit case — a later RUN *misses* after an earlier step was
    /// adopted — would otherwise execute against an incomplete filesystem;
    /// [`Self::adopted_from_cache`] flags that this has happened so
    /// `build_one_stage` can rebuild the stage with cache lookups disabled
    /// (every step re-executes, repopulating `upper_guards` correctly).
    pub(crate) fn adopt_cached_layer(&mut self, layer: LayerSource, history_line: String) {
        self.adopted_from_cache = true;
        self.new_layers.push(layer);
        self.image_config.history.push(HistoryEntry {
            created: Some(now_rfc3339()),
            created_by: Some(history_line),
            author: Some("umf-engine".to_string()),
            comment: None,
            empty_layer: false,
        });
    }

    /// Append an empty-layer history entry (metadata-only step like
    /// `LABEL` or `ENV`). No new layer is produced.
    pub(crate) fn push_metadata_history(&mut self, history_line: String) {
        self.image_config.history.push(HistoryEntry {
            created: Some(now_rfc3339()),
            created_by: Some(history_line),
            author: Some("umf-engine".to_string()),
            comment: None,
            empty_layer: true,
        });
    }
}

/// Test-only constructors shared by the `directives` and `units` test
/// modules — both build an empty [`BuildState`] and inspect the last
/// synthesised upper-dir.
#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
