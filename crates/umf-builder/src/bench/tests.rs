//! Unit tests for the `bench` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

fn metrics_with_total(ms: u128) -> BuildMetrics {
    BuildMetrics {
        total_ms: ms,
        layer_count: Some(3),
        total_layer_bytes: Some(1024),
        ..BuildMetrics::default()
    }
}

#[test]
fn percentile_handles_empty_and_single() {
    assert_eq!(percentile(&[], 50.0), 0);
    assert_eq!(percentile(&[42], 50.0), 42);
    assert_eq!(percentile(&[42], 99.0), 42);
}

#[test]
fn percentile_median_picks_middle() {
    let sorted = vec![10, 20, 30, 40, 50];
    assert_eq!(percentile(&sorted, 50.0), 30);
}

#[test]
fn percentile_p99_picks_near_max() {
    let sorted: Vec<u128> = (1..=100).collect();
    assert!(percentile(&sorted, 99.0) >= 99);
}

#[test]
fn summarise_reports_min_max_mean() {
    let runs = vec![
        metrics_with_total(100),
        metrics_with_total(200),
        metrics_with_total(150),
    ];
    let s = summarise(&runs);
    assert_eq!(s.n, 3);
    assert_eq!(s.total_ms.min, 100);
    assert_eq!(s.total_ms.max, 200);
    assert!((s.total_ms.mean - 150.0).abs() < 0.1);
    assert_eq!(s.total_ms.median, 150);
    // Deterministic recipe → no variance flags.
    assert_eq!(s.layer_count_range, (3, 3));
    assert_eq!(s.total_layer_bytes_range, (1024, 1024));
}

#[test]
fn summarise_flags_layer_count_variance() {
    let runs = vec![
        metrics_with_total(100),
        BuildMetrics {
            total_ms: 110,
            layer_count: Some(4),
            total_layer_bytes: Some(2048),
            ..BuildMetrics::default()
        },
    ];
    let s = summarise(&runs);
    assert_eq!(s.layer_count_range, (3, 4));
    assert_eq!(s.total_layer_bytes_range, (1024, 2048));
}

#[test]
fn aggregate_no_warm_runs_yields_no_summary() {
    let cold = metrics_with_total(500);
    let report = BenchReport::aggregate("/tmp/recipe.umf", 0, Some(cold), vec![]);
    assert!(report.warm_summary.is_none());
    assert_eq!(report.runs, 0);
}

#[test]
fn aggregate_with_warm_runs_yields_summary() {
    let cold = metrics_with_total(500);
    let warm = vec![
        metrics_with_total(100),
        metrics_with_total(120),
        metrics_with_total(110),
    ];
    let report = BenchReport::aggregate("/tmp/recipe.umf", 0, Some(cold), warm);
    let s = report.warm_summary.expect("summary set");
    assert_eq!(s.n, 3);
    assert_eq!(s.total_ms.min, 100);
    assert_eq!(s.total_ms.max, 120);
}

#[test]
fn render_text_contains_cold_and_warm_lines() {
    let cold = metrics_with_total(500);
    let warm = vec![metrics_with_total(100), metrics_with_total(110)];
    let report = BenchReport::aggregate("/tmp/r.umf", 1, Some(cold), warm);
    let text = report.render_text();
    assert!(text.contains("Bench report"));
    assert!(text.contains("/tmp/r.umf"));
    assert!(text.contains("cold:"));
    assert!(text.contains("warm (2 runs)"));
    assert!(text.contains("median"));
}

#[test]
fn render_text_flags_variance() {
    let warm = vec![
        metrics_with_total(100),
        BuildMetrics {
            total_ms: 110,
            layer_count: Some(5),
            total_layer_bytes: Some(2048),
            ..BuildMetrics::default()
        },
    ];
    let report = BenchReport::aggregate("/tmp/r.umf", 0, None, warm);
    let text = report.render_text();
    assert!(text.contains("⚠ layer count varied"));
    assert!(text.contains("⚠ total layer bytes varied"));
}
