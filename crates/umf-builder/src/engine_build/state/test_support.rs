//! Unit tests for the `state` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use super::{BaseImage, BuildState};
use umf_oci::image::ImageConfig;

pub(crate) fn empty_state() -> BuildState {
    let base = BaseImage {
        layers: Vec::new(),
        config: ImageConfig::default(),
    };
    BuildState::new(
        base,
        umf_oci::image::LayerCompression::Gzip,
        &BTreeMap::new(),
    )
}
