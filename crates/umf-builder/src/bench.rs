//! Build-benchmark harness — aggregate `BuildMetrics` across N runs.
//!
//! Consumes the per-build `BuildMetrics` from
//! [`crate::metrics::BuildMetrics`] and produces a `BenchReport` that
//! captures one cold-cache run + N warm-cache runs with median /
//! p99 / min / max for the total wall-clock time.
//!
//! The harness is split between **this module** (pure aggregation +
//! serialisation, no IO) and the `umf` CLI's `run_bench` (drives the
//! actual build invocations + writes per-run reports to disk). The
//! split keeps the aggregation logic unit-testable without spawning
//! any builds.

use serde::Serialize;

use crate::metrics::{BuildMetrics, human_bytes};

/// Full bench report — every run's measurements + aggregated stats
/// over the warm runs.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// Recipe path the bench measured.
    pub recipe: String,
    /// Configured number of warm runs (excludes the cold run).
    pub runs: usize,
    /// Configured number of pre-measurement warmup runs.
    pub warmup: usize,
    /// Cold-cache run — `None` when the bench was configured to skip
    /// the cold run (rare; mostly useful for warm-only benchmarks
    /// against an already-populated cache).
    pub cold_run: Option<BuildMetrics>,
    /// Per-warm-run measurements, in chronological order.
    pub warm_runs: Vec<BuildMetrics>,
    /// Aggregated stats over `warm_runs`. `None` when there were no
    /// warm runs (e.g. `--cold-only`).
    pub warm_summary: Option<BenchSummary>,
}

/// Aggregate stats over a set of runs.
#[derive(Debug, Clone, Serialize)]
pub struct BenchSummary {
    /// Number of runs the stats were computed over.
    pub n: usize,
    /// Wall-clock total in milliseconds.
    pub total_ms: PercentileStats,
    /// Layer count — should be invariant across warm runs of a
    /// deterministic recipe. Reported as `(min, max)` so non-zero
    /// variance is immediately visible.
    pub layer_count_range: (usize, usize),
    /// Total compressed bytes — same deterministic-recipe expectation
    /// as `layer_count_range`.
    pub total_layer_bytes_range: (i64, i64),
}

/// Five-number summary for a series of u128 measurements.
#[derive(Debug, Clone, Serialize)]
pub struct PercentileStats {
    /// Minimum observed value.
    pub min: u128,
    /// Maximum observed value.
    pub max: u128,
    /// Arithmetic mean.
    pub mean: f64,
    /// 50th percentile (linear interpolation).
    pub median: u128,
    /// 99th percentile (linear interpolation).
    pub p99: u128,
}

impl BenchReport {
    /// Build a report from a cold-run measurement plus a series of
    /// warm-run measurements.
    #[must_use]
    pub fn aggregate(
        recipe: impl Into<String>,
        warmup: usize,
        cold_run: Option<BuildMetrics>,
        warm_runs: Vec<BuildMetrics>,
    ) -> Self {
        let warm_summary = if warm_runs.is_empty() {
            None
        } else {
            Some(summarise(&warm_runs))
        };
        Self {
            recipe: recipe.into(),
            runs: warm_runs.len(),
            warmup,
            cold_run,
            warm_runs,
            warm_summary,
        }
    }

    /// Render the report as a human-readable table.
    #[must_use]
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "Bench report — {}", self.recipe);
        let _ = writeln!(
            out,
            "  runs: cold + {} warm{}",
            self.runs,
            if self.warmup > 0 {
                format!(" (with {} warmup)", self.warmup)
            } else {
                String::new()
            }
        );
        if let Some(cold) = &self.cold_run {
            let _ = writeln!(out, "─────────────");
            let _ = writeln!(
                out,
                "cold: {:>7} ms   {} layer(s)   {} compressed",
                cold.total_ms,
                cold.layer_count.unwrap_or(0),
                human_bytes(cold.total_layer_bytes.unwrap_or(0)),
            );
        }
        if let Some(s) = &self.warm_summary {
            let _ = writeln!(out, "─────────────");
            let _ = writeln!(
                out,
                "warm ({} runs): median {} ms  p99 {} ms  min {}  max {}  mean {:.1}",
                s.n,
                s.total_ms.median,
                s.total_ms.p99,
                s.total_ms.min,
                s.total_ms.max,
                s.total_ms.mean,
            );
            if s.layer_count_range.0 != s.layer_count_range.1 {
                let _ = writeln!(
                    out,
                    "  ⚠ layer count varied across warm runs: {} .. {}",
                    s.layer_count_range.0, s.layer_count_range.1,
                );
            }
            if s.total_layer_bytes_range.0 != s.total_layer_bytes_range.1 {
                let _ = writeln!(
                    out,
                    "  ⚠ total layer bytes varied across warm runs: {} .. {}",
                    human_bytes(s.total_layer_bytes_range.0),
                    human_bytes(s.total_layer_bytes_range.1),
                );
            }
        }
        out
    }
}

/// Compute the [`BenchSummary`] over the supplied warm runs.
/// Internal helper — exposed for tests.
#[must_use]
pub fn summarise(runs: &[BuildMetrics]) -> BenchSummary {
    let mut totals: Vec<u128> = runs.iter().map(|r| r.total_ms).collect();
    totals.sort_unstable();

    let n = totals.len();
    let sum: u128 = totals.iter().sum();
    let mean = sum as f64 / n as f64;
    let min = *totals.first().unwrap_or(&0);
    let max = *totals.last().unwrap_or(&0);
    let median = percentile(&totals, 50.0);
    let p99 = percentile(&totals, 99.0);

    let layer_counts: Vec<usize> = runs.iter().filter_map(|r| r.layer_count).collect();
    let layer_count_range = if layer_counts.is_empty() {
        (0, 0)
    } else {
        (
            *layer_counts.iter().min().unwrap_or(&0),
            *layer_counts.iter().max().unwrap_or(&0),
        )
    };

    let layer_bytes: Vec<i64> = runs.iter().filter_map(|r| r.total_layer_bytes).collect();
    let total_layer_bytes_range = if layer_bytes.is_empty() {
        (0, 0)
    } else {
        (
            *layer_bytes.iter().min().unwrap_or(&0),
            *layer_bytes.iter().max().unwrap_or(&0),
        )
    };

    BenchSummary {
        n,
        total_ms: PercentileStats {
            min,
            max,
            mean,
            median,
            p99,
        },
        layer_count_range,
        total_layer_bytes_range,
    }
}

/// Pick the value at the `p` percentile of a *sorted* slice.
/// Linear interpolation; `p` is a percentage in 0..=100.
#[must_use]
pub fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let rank = (p / 100.0) * (n as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = rank - lo as f64;
        let lo_v = sorted[lo] as f64;
        let hi_v = sorted[hi] as f64;
        (lo_v + (hi_v - lo_v) * frac) as u128
    }
}

#[cfg(test)]
mod tests;
