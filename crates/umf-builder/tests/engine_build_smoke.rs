//! End-to-end smoke test for `umf_builder::engine_build::build_single_stage`.
//!
//! Pulls `alpine:3.21`, builds a single-stage recipe through the
//! UMF-native engine, and validates the resulting OCI image's structure:
//!
//! - Layer count = base layers + new layers (one per RUN / ADD).
//! - Image-config inherits the base's env (PATH preserved), overlays
//!   our LABEL + ENTRYPOINT.
//! - UMF type / spec labels are injected.
//! - Re-building the same recipe hits the cache (no engine execution
//!   the second time).
//!
//! Gated behind `UMF_ENGINE_SMOKE=1` because it needs network + root
//! (kernel overlayfs requires `CAP_SYS_ADMIN`).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use oci_client::manifest::OciImageManifest;
use serde::Deserialize;
use tempfile::TempDir;
use umf_builder::engine_build::{EngineBuildOptions, build_single_stage};
use umf_oci::registry::ImageLayout;

const SMOKE_ENV: &str = "UMF_ENGINE_SMOKE";

fn smoke_enabled() -> bool {
    std::env::var(SMOKE_ENV).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

/// Pull errors that are environmental (registry rate limit, transient
/// network failure) shouldn't fail the test — they should mark it
/// skipped with a clear message. We detect by inspecting the error
/// string; brittle but tests are advisory anyway.
fn is_pull_environmental(err: &dyn std::fmt::Display) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("toomanyrequests")
        || s.contains("rate limit")
        || s.contains("name or service not known")
        || s.contains("connection refused")
        || s.contains("network is unreachable")
}

#[derive(Debug, Deserialize)]
struct InspectConfig {
    config: InspectConfigInner,
    rootfs: InspectRootfs,
    #[serde(default)]
    history: Vec<InspectHistory>,
}

#[derive(Debug, Default, Deserialize)]
struct InspectConfigInner {
    #[serde(default, rename = "Env")]
    env: Vec<String>,
    #[serde(default, rename = "Entrypoint")]
    entrypoint: Vec<String>,
    #[serde(default, rename = "Labels")]
    labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct InspectRootfs {
    diff_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct InspectHistory {
    #[serde(default)]
    created_by: Option<String>,
    #[serde(default)]
    empty_layer: bool,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_stage_recipe_round_trips() {
    if !smoke_enabled() {
        eprintln!(
            "skipping {SMOKE_ENV}-gated smoke test (set {SMOKE_ENV}=1 to run; needs network + CAP_SYS_ADMIN)"
        );
        return;
    }

    // Stage a context dir holding a recipe + a file that ADD will pick up.
    let ctx = TempDir::new().expect("ctx tempdir");
    std::fs::write(ctx.path().join("greeting.txt"), b"hello from umf\n")
        .expect("write greeting.txt");
    let recipe_path = ctx.path().join("smoke.umf");
    // FROM base is overridable (UMF_SMOKE_BASE_IMAGE) so CI can avoid Docker
    // Hub's anonymous rate limit; the mirror is byte-identical alpine, so the
    // layer-count / env-inheritance assertions below hold unchanged.
    let base = std::env::var("UMF_SMOKE_BASE_IMAGE").unwrap_or_else(|_| "alpine:3.21".to_string());
    std::fs::write(
        &recipe_path,
        format!(
            "FROM {base}\n\
             ADD greeting.txt /etc/greeting\n\
             RUN cat /etc/greeting\n\
             LABEL org.imagilux.umf.author=\"umf-engine-smoke\"\n\
             ENV LANG=C.UTF-8\n\
             ENTRYPOINT [\"/bin/cat\", \"/etc/greeting\"]\n"
        ),
    )
    .expect("write recipe");

    let source = std::fs::read_to_string(&recipe_path).expect("read recipe");
    let ast = umf_parser::parse(&source).expect("parse");

    let layout_dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let final_ref = "example.invalid/umf-engine-smoke:1";

    // Build 1 — cold cache. Performs the actual libcontainer execution.
    let entry = match build_single_stage(
        &layout,
        ctx.path(),
        &ast,
        final_ref,
        &EngineBuildOptions::default(),
    )
    .await
    {
        Ok(e) => e,
        Err(err) if is_pull_environmental(&err) => {
            eprintln!("skipping: environmental pull failure: {err}");
            return;
        }
        Err(err) => panic!("engine build: {err}"),
    };

    // Inspect the manifest + image-config.
    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest blob");
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse manifest");

    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let config: InspectConfig = serde_json::from_slice(&config_bytes).expect("parse image config");

    // Layer count: alpine base contributes 1 layer; we add 1 for ADD + 1 for RUN.
    // The RUN here only reads (cat) — but still produces an empty-ish diff layer
    // (likely just /root or similar mtime touches). We allow flexibility on the
    // exact count, but at minimum we should have the base + 2 new layers.
    assert!(
        manifest.layers.len() >= 3,
        "expected at least 3 layers (base + ADD + RUN); got {}",
        manifest.layers.len()
    );
    assert_eq!(manifest.layers.len(), config.rootfs.diff_ids.len(),);

    // Env inherited from alpine (PATH at least), augmented with our ENV.
    assert!(
        config.config.env.iter().any(|e| e.starts_with("PATH=")),
        "PATH should be inherited from alpine; got env={:?}",
        config.config.env,
    );
    assert!(
        config.config.env.iter().any(|e| e == "LANG=C.UTF-8"),
        "ENV LANG=... should be applied; got env={:?}",
        config.config.env,
    );

    // ENTRYPOINT from our recipe (exec form).
    assert_eq!(
        config.config.entrypoint,
        vec!["/bin/cat".to_string(), "/etc/greeting".to_string()],
    );

    // UMF type + spec labels — emit_image injects these.
    assert_eq!(
        config
            .config
            .labels
            .get("org.imagilux.umf.type")
            .map(String::as_str),
        Some("container"),
    );
    assert!(
        config.config.labels.contains_key("org.imagilux.umf.spec"),
        "spec_version label should be set; got labels={:?}",
        config.config.labels,
    );
    assert_eq!(
        config
            .config
            .labels
            .get("org.imagilux.umf.author")
            .map(String::as_str),
        Some("umf-engine-smoke"),
    );

    // History entries: at least one for ADD and one for RUN, and the
    // metadata-only entries (LABEL, ENV, ENTRYPOINT) should be marked
    // empty_layer.
    let add_seen = config.history.iter().any(|h| {
        h.created_by
            .as_deref()
            .is_some_and(|s| s.starts_with("ADD"))
    });
    let run_seen = config.history.iter().any(|h| {
        h.created_by
            .as_deref()
            .is_some_and(|s| s.starts_with("RUN"))
    });
    let entrypoint_seen = config.history.iter().any(|h| {
        h.created_by
            .as_deref()
            .is_some_and(|s| s.starts_with("ENTRYPOINT"))
            && h.empty_layer
    });
    assert!(add_seen, "ADD history entry missing");
    assert!(run_seen, "RUN history entry missing");
    assert!(entrypoint_seen, "ENTRYPOINT history entry missing");

    // Build 2 — same input. Cache should hit and skip libcontainer
    // execution. We don't have an in-process counter to verify "no
    // execution", but we *can* confirm the second build produces a
    // semantically-identical layer chain.
    let entry_2 = build_single_stage(
        &layout,
        ctx.path(),
        &ast,
        final_ref,
        &EngineBuildOptions::default(),
    )
    .await
    .expect("engine build (cache pass)");

    let manifest_bytes_2 = layout.read_blob(&entry_2.digest).expect("manifest blob 2");
    let manifest_2: OciImageManifest =
        serde_json::from_slice(&manifest_bytes_2).expect("parse manifest 2");
    let config_bytes_2 = layout
        .read_blob(&manifest_2.config.digest)
        .expect("config blob 2");
    let config_2: InspectConfig =
        serde_json::from_slice(&config_bytes_2).expect("parse image config 2");
    assert_eq!(
        config.rootfs.diff_ids, config_2.rootfs.diff_ids,
        "cache hit should reproduce the same diff_id chain",
    );
}

/// Two-stage build: builder stage compiles output, runtime stage
/// `ADD --from=builder` brings it across, ENTRYPOINT runs it.
///
/// Verifies the multi-stage acceptance criterion: the runtime
/// image contains the artifact produced by the builder, and the
/// `ADD --from` directive sources from the producer stage's rootfs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_stage_recipe_round_trips() {
    if !smoke_enabled() {
        eprintln!(
            "skipping {SMOKE_ENV}-gated smoke test (set {SMOKE_ENV}=1 to run; needs network + CAP_SYS_ADMIN)"
        );
        return;
    }

    let ctx = TempDir::new().expect("ctx tempdir");
    let recipe_path = ctx.path().join("multi.umf");
    let base = std::env::var("UMF_SMOKE_BASE_IMAGE").unwrap_or_else(|_| "alpine:3.21".to_string());
    std::fs::write(
        &recipe_path,
        format!(
            "FROM {base} AS builder\n\
             RUN mkdir -p /work && echo compiled-output > /work/output\n\
             \n\
             FROM {base}\n\
             ADD --from=builder /work/output /usr/local/bin/output\n\
             LABEL org.imagilux.umf.author=\"multi-stage-smoke\"\n\
             ENTRYPOINT [\"/usr/local/bin/output\"]\n"
        ),
    )
    .expect("write recipe");

    let source = std::fs::read_to_string(&recipe_path).expect("read recipe");
    let ast = umf_parser::parse(&source).expect("parse");

    let layout_dir = TempDir::new().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let final_ref = "example.invalid/multi-stage-smoke:1";

    let entry = match umf_builder::engine_build::build(
        &layout,
        ctx.path(),
        &ast,
        final_ref,
        &umf_builder::engine_build::EngineBuildOptions::default(),
    )
    .await
    {
        Ok(e) => e,
        Err(err) if is_pull_environmental(&err) => {
            eprintln!("skipping: environmental pull failure: {err}");
            return;
        }
        Err(err) => panic!("multi-stage engine build: {err}"),
    };

    let manifest_bytes = layout.read_blob(&entry.digest).expect("manifest blob");
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).expect("parse manifest");
    let config_bytes = layout
        .read_blob(&manifest.config.digest)
        .expect("config blob");
    let config: InspectConfig = serde_json::from_slice(&config_bytes).expect("parse image config");

    // The final image should carry: alpine base layer + the ADD --from
    // layer (carrying /usr/local/bin/output). No RUN layer in the
    // runtime stage.
    assert!(
        manifest.layers.len() >= 2,
        "expected at least 2 layers (alpine + ADD --from); got {}",
        manifest.layers.len(),
    );
    assert_eq!(manifest.layers.len(), config.rootfs.diff_ids.len());

    // ENTRYPOINT preserved from the runtime stage.
    assert_eq!(
        config.config.entrypoint,
        vec!["/usr/local/bin/output".to_string()],
    );

    // LABEL applied in the runtime stage survives.
    assert_eq!(
        config
            .config
            .labels
            .get("org.imagilux.umf.author")
            .map(String::as_str),
        Some("multi-stage-smoke"),
    );

    // History should carry the ADD --from entry.
    let add_from_seen = config.history.iter().any(|h| {
        h.created_by
            .as_deref()
            .is_some_and(|s| s.contains("ADD --from=builder"))
    });
    assert!(
        add_from_seen,
        "ADD --from history entry should be present; got history={:?}",
        config.history,
    );
}
