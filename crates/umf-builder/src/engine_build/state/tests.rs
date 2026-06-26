//! Unit tests for the `state` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn adopt_cached_layer_flags_partial_hit_hazard() {
    let mut state = test_support::empty_state();
    assert!(!state.adopted_from_cache, "fresh state has adopted nothing");

    let td = tempfile::tempdir().expect("tempdir");
    std::fs::write(td.path().join("f"), b"x").expect("write");
    let layer = LayerSource::from_directory(td.path()).expect("layer from dir");

    state.adopt_cached_layer(layer, "RUN cached".to_string());

    assert!(
        state.adopted_from_cache,
        "adopting a cached layer must set the partial-hit hazard flag"
    );
    // The adopted layer is recorded in the chain but has NO upper-dir guard
    // — exactly the asymmetry that makes a later RUN's overlay incomplete,
    // which `build_one_stage` detects via this flag and rebuilds around.
    assert_eq!(state.new_layers.len(), 1);
    assert!(state.upper_guards.is_empty());
}
