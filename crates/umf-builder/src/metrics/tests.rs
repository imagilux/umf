//! Unit tests for the `metrics` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn builder_finish_records_total_ms() {
    let builder = BuildMetrics::start();
    // Sleep briefly so total_ms is > 0.
    std::thread::sleep(std::time::Duration::from_millis(10));
    let m = builder.finish();
    assert!(m.total_ms >= 10, "got: {}", m.total_ms);
    assert_eq!(m.layer_count, None);
}

#[test]
fn finish_with_image_captures_layer_stats() {
    let builder = BuildMetrics::start();
    let m = builder.finish_with_image(3, 12_345, Some(8_000));
    assert_eq!(m.layer_count, Some(3));
    assert_eq!(m.total_layer_bytes, Some(12_345));
    assert_eq!(m.pushed_bytes, Some(8_000));
}

#[test]
fn render_text_includes_total_and_phases() {
    let mut m = BuildMetrics {
        total_ms: 1234,
        layer_count: Some(2),
        total_layer_bytes: Some(2048),
        pushed_bytes: Some(512),
        ..BuildMetrics::default()
    };
    m.phases_ms.insert("umf.parse".to_string(), 5);
    m.phase_counts.insert("umf.parse".to_string(), 1);
    m.phases_ms.insert("umf.engine.run_step".to_string(), 800);
    m.phase_counts.insert("umf.engine.run_step".to_string(), 3);

    let text = m.render_text();
    assert!(text.contains("Build summary"));
    assert!(text.contains("umf.parse"));
    assert!(text.contains("5 ms"));
    assert!(text.contains("umf.engine.run_step"));
    assert!(text.contains("(3× total"));
    assert!(text.contains("total: 1234 ms"));
    assert!(text.contains("2 layer(s)"));
    assert!(text.contains("pushed"));
}

#[test]
fn human_bytes_thresholds() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(1024), "1.00 Ki");
    assert_eq!(human_bytes(1_500_000), "1.43 Mi");
    assert!(human_bytes(2_000_000_000).contains("Gi"));
}

#[test]
fn render_text_without_image_or_push() {
    let m = BuildMetrics {
        total_ms: 42,
        ..BuildMetrics::default()
    };
    let text = m.render_text();
    assert!(text.contains("total: 42 ms"));
    assert!(!text.contains("layer(s)"));
    assert!(!text.contains("pushed"));
}
