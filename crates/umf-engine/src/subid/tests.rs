//! Unit tests for the `subid` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn parse_subid_range_matches_username_then_uid() {
    let body = "# delegated ranges\nalice:100000:65536\nbob:200000:65536\n1000:300000:1000\n";
    assert_eq!(
        parse_subid_range(body, Some("bob"), 4242),
        Some(SubIdRange {
            start: 200_000,
            count: 65_536
        })
    );
    // Falls back to a numeric-uid match when the username doesn't appear.
    assert_eq!(
        parse_subid_range(body, Some("carol"), 1000),
        Some(SubIdRange {
            start: 300_000,
            count: 1000
        })
    );
    // No match; zero-count ranges are ignored; comments/blanks skipped.
    assert_eq!(parse_subid_range(body, Some("dave"), 5), None);
    assert_eq!(parse_subid_range("eve:1:0\n", Some("eve"), 5), None);
    // First match wins for a user with multiple lines.
    assert_eq!(
        parse_subid_range("x:10:5\nx:20:5\n", Some("x"), 0),
        Some(SubIdRange {
            start: 10,
            count: 5
        })
    );
}

#[test]
fn mapping_triples_maps_root_to_user_then_the_range() {
    let sub_uid = SubIdRange {
        start: 100_000,
        count: 65_536,
    };
    let sub_gid = SubIdRange {
        start: 200_000,
        count: 1000,
    };
    let (uid_t, gid_t) = mapping_triples(1000, 1001, &sub_uid, &sub_gid);
    // container 0 -> the invoking user (size 1), then container 1 -> the range.
    assert_eq!(uid_t, vec![(0, 1000, 1), (1, 100_000, 65_536)]);
    assert_eq!(gid_t, vec![(0, 1001, 1), (1, 200_000, 1000)]);
}

#[test]
fn helper_args_flatten_pid_then_triples() {
    let triples = [(0u32, 1000u32, 1u32), (1, 100_000, 65_536)];
    assert_eq!(
        helper_args(4242, &triples),
        vec!["4242", "0", "1000", "1", "1", "100000", "65536"]
    );
    // Argv shape newuidmap/newgidmap expect: <pid> then flat (cid hid size) runs.
    assert_eq!(helper_args(7, &[]), vec!["7"]);
}
