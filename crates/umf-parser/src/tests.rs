//! Unit tests for the `umf-parser` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn over_cap_source_is_rejected() {
    // One byte over the cap. The size gate runs before tokenization, so
    // this returns immediately with a single size-limit diagnostic rather
    // than allocating proportionally to the input.
    let big = "a".repeat(MAX_SOURCE_BYTES + 1);
    let errs = parse(&big).expect_err("over-cap source must be rejected");
    assert_eq!(errs.len(), 1, "exactly one size-limit diagnostic");
}
