//! Per-build metrics — timing + layer composition + cache stats.
//!
//! Consumes the structured spans emitted across the pipeline. A
//! [`MetricsLayer`] sits in the tracing subscriber stack and records
//! enter/close times for any span whose name starts with `umf.`; on
//! build completion the caller (typically the `umf` CLI) finalises a
//! [`BuildMetrics`] with the recorded phase timings plus
//! manifest-derived stats (layer count, total bytes).
//!
//! ## Usage
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use umf_builder::metrics::{BuildMetrics, MetricsLayer};
//! # use tracing_subscriber::layer::SubscriberExt;
//! let metrics = BuildMetrics::start();
//! let layer = MetricsLayer::new(metrics.shared());
//!
//! // Compose with the existing subscriber stack.
//! let subscriber = tracing_subscriber::registry().with(layer);
//! // ... install subscriber, run build ...
//!
//! let final_metrics = metrics.finish_with_image(
//!     0 /* layer count */,
//!     0 /* total layer bytes */,
//!     None /* pushed_bytes — Some(n) when --push */,
//! );
//! println!("{}", final_metrics.render_text());
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::Subscriber;
use tracing::span::{Attributes, Id};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Collected per-build measurements. Built up by the
/// [`MetricsLayer`] (phase timings) + the caller's `finish_with_*`
/// methods (image-shape stats).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BuildMetrics {
    /// Wall-clock duration of the whole build, in milliseconds.
    /// Set by [`MetricsBuilder::finish`].
    pub total_ms: u128,
    /// Per-phase timings keyed by span name (`umf.parse`,
    /// `umf.engine.build`, ...). For spans that fire multiple times
    /// (per-stage, per-step), this map carries the **last** observed
    /// duration; aggregate counts live in [`Self::phase_counts`].
    pub phases_ms: BTreeMap<String, u128>,
    /// Number of times each span name was observed during the build.
    pub phase_counts: BTreeMap<String, u64>,
    /// Layer count on the final manifest. `None` until
    /// `finish_with_image` is called.
    pub layer_count: Option<usize>,
    /// Total compressed bytes across all final-manifest layers.
    /// `None` until `finish_with_image` is called.
    pub total_layer_bytes: Option<i64>,
    /// Bytes uploaded during the push phase. `None` when the build
    /// didn't push (or the caller didn't pass push info).
    pub pushed_bytes: Option<i64>,
}

impl BuildMetrics {
    /// Start a new metrics collector. Returns a [`MetricsBuilder`] the
    /// caller drives through the build lifecycle.
    #[must_use]
    pub fn start() -> MetricsBuilder {
        MetricsBuilder {
            shared: Arc::new(Mutex::new(BuildMetrics::default())),
            started_at: Instant::now(),
        }
    }

    /// Render a column-aligned "Build summary" table — what `umf build`
    /// prints by default at end of a build.
    #[must_use]
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "Build summary");
        let _ = writeln!(out, "─────────────");
        for (phase, ms) in &self.phases_ms {
            let count = self.phase_counts.get(phase).copied().unwrap_or(1);
            let suffix = if count > 1 {
                format!("  ({count}× total, last)")
            } else {
                String::new()
            };
            let _ = writeln!(out, "{phase:<30} {ms:>6} ms{suffix}");
        }
        if let Some(layers) = self.layer_count {
            let _ = writeln!(out, "─────────────");
            let bytes = self.total_layer_bytes.unwrap_or(0);
            let _ = writeln!(
                out,
                "image: {layers} layer(s), {} compressed bytes",
                human_bytes(bytes),
            );
        }
        if let Some(pushed) = self.pushed_bytes {
            let _ = writeln!(out, "push: {} pushed", human_bytes(pushed));
        }
        let _ = writeln!(out, "─────────────");
        let _ = writeln!(out, "total: {} ms", self.total_ms);
        out
    }
}

/// Live metrics being collected during a build. Cheap to clone — both
/// the [`MetricsLayer`] subscriber and the caller share access via
/// [`Self::shared`].
pub struct MetricsBuilder {
    shared: Arc<Mutex<BuildMetrics>>,
    started_at: Instant,
}

impl MetricsBuilder {
    /// Handle the [`MetricsLayer`] writes through. The layer + the
    /// caller both hold an [`Arc`] to the same inner [`BuildMetrics`].
    #[must_use]
    pub fn shared(&self) -> Arc<Mutex<BuildMetrics>> {
        Arc::clone(&self.shared)
    }

    /// Finalise the metrics with image-shape stats from the produced
    /// manifest, then return the full [`BuildMetrics`].
    #[must_use]
    pub fn finish_with_image(
        self,
        layer_count: usize,
        total_layer_bytes: i64,
        pushed_bytes: Option<i64>,
    ) -> BuildMetrics {
        let total = self.started_at.elapsed();
        let shared = self.shared;
        let mut m = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        m.total_ms = total.as_millis();
        m.layer_count = Some(layer_count);
        m.total_layer_bytes = Some(total_layer_bytes);
        m.pushed_bytes = pushed_bytes;
        m
    }

    /// Finalise without image stats — for builds that failed or that
    /// don't produce a manifest (e.g. parse error before build).
    #[must_use]
    pub fn finish(self) -> BuildMetrics {
        let total = self.started_at.elapsed();
        let mut m = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        m.total_ms = total.as_millis();
        m
    }
}

/// `tracing::Layer` that captures the enter/close timestamps of every
/// span whose name starts with `umf.` and records the elapsed time
/// into a shared [`BuildMetrics`].
///
/// Compose into the subscriber stack alongside the formatting layer:
///
/// ```no_run
/// # use std::sync::{Arc, Mutex};
/// # use umf_builder::metrics::{BuildMetrics, MetricsLayer};
/// # use tracing_subscriber::layer::SubscriberExt;
/// # let shared = Arc::new(Mutex::new(BuildMetrics::default()));
/// let layer = MetricsLayer::new(shared);
/// let subscriber = tracing_subscriber::registry().with(layer);
/// ```
pub struct MetricsLayer {
    metrics: Arc<Mutex<BuildMetrics>>,
    span_starts: Mutex<BTreeMap<u64, Instant>>,
}

impl MetricsLayer {
    /// Build a layer that writes into `metrics`.
    #[must_use]
    pub fn new(metrics: Arc<Mutex<BuildMetrics>>) -> Self {
        Self {
            metrics,
            span_starts: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<S> Layer<S> for MetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        if name.starts_with("umf.") {
            let mut starts = self
                .span_starts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            starts.insert(id.into_u64(), Instant::now());
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let start_opt = {
            let mut starts = self
                .span_starts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            starts.remove(&id.into_u64())
        };
        let Some(start) = start_opt else { return };
        let Some(span) = ctx.span(&id) else { return };
        let name = span.name().to_string();
        let elapsed_ms = start.elapsed().as_millis();
        let mut m = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        m.phases_ms.insert(name.clone(), elapsed_ms);
        *m.phase_counts.entry(name).or_insert(0) += 1;
    }
}

/// Format `bytes` as `<N.NN> Ki/Mi/Gi`, leaving small values as raw.
///
/// Shared by [`crate::bench`]'s report renderer too — both formatters were
/// previously byte-identical copies.
pub(crate) fn human_bytes(bytes: i64) -> String {
    let bytes = bytes.max(0) as f64;
    const KI: f64 = 1024.0;
    const MI: f64 = KI * 1024.0;
    const GI: f64 = MI * 1024.0;
    if bytes >= GI {
        format!("{:.2} Gi", bytes / GI)
    } else if bytes >= MI {
        format!("{:.2} Mi", bytes / MI)
    } else if bytes >= KI {
        format!("{:.2} Ki", bytes / KI)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests;
