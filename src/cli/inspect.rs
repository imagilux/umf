//! `umf inspect` — resolve an OCI artifact (pulling on miss) and report
//! its UMF labels, target type, runtime config, layer composition, and
//! history as either a table or structured JSON.

use std::path::Path;

use oci_client::Reference;
use thiserror::Error;
use tracing::info;
use umf_oci::registry::{ImageLayout, RegistryError};

use crate::cli::InspectFormat;
use crate::cli::util::{self, CredentialError};

#[derive(Debug, Error)]
pub(crate) enum CliInspectError {
    #[error("layout dir: {0}")]
    LayoutDir(String),
    #[error("read password from stdin: {0}")]
    PasswordStdin(std::io::Error),
    #[error("--password-stdin requires --username")]
    PasswordStdinWithoutUsername,
    #[error("invalid OCI reference {reference:?}: {err}")]
    BadReference {
        reference: String,
        err: oci_client::ParseError,
    },
    #[error("invalid --platform: {0}")]
    BadPlatform(umf_core::architecture::PlatformParseError),
    #[error(
        "image index {reference} has no manifest for {arch}; available: {available} (build that arch first, or pass a different --platform)"
    )]
    NoManifestForPlatform {
        reference: String,
        arch: String,
        available: String,
    },
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest / config JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<CredentialError> for CliInspectError {
    fn from(err: CredentialError) -> Self {
        match err {
            CredentialError::PasswordStdinWithoutUsername => Self::PasswordStdinWithoutUsername,
            CredentialError::PasswordStdin(e) => Self::PasswordStdin(e),
        }
    }
}

/// Bundled `umf inspect` flags.
pub(crate) struct InspectArgs<'a> {
    pub(crate) reference: &'a str,
    pub(crate) format: InspectFormat,
    pub(crate) show_blobs: bool,
    /// `--platform` (`os/arch`). When the reference resolves to a multi-arch
    /// image index, selects which child to report; defaults to the host arch.
    /// Ignored for a single-arch image.
    pub(crate) platform: Option<&'a str>,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) insecure_registry: bool,
    pub(crate) username: Option<&'a str>,
    pub(crate) password_stdin: bool,
}

/// Resolve the image (pulling on miss), build the report, and render
/// it as a table or JSON.
pub(crate) fn run_inspect(args: InspectArgs<'_>) -> Result<(), CliInspectError> {
    let layout_dir = match args.layout_dir_override {
        Some(p) => p.to_path_buf(),
        None => util::default_layout_dir().map_err(CliInspectError::LayoutDir)?,
    };
    let layout = ImageLayout::init(&layout_dir)?;
    info!(layout = %layout_dir.display(), "layout ready");

    let reference: Reference = args
        .reference
        .parse()
        .map_err(
            |err: oci_client::ParseError| CliInspectError::BadReference {
                reference: args.reference.to_string(),
                err,
            },
        )?;

    // `--platform` selects the child to report when the reference resolves to
    // a multi-arch index. Default to the host arch (matches the build / run
    // consume paths). A single-arch image ignores it.
    let architecture = match args.platform {
        Some(p) => umf_core::architecture::Architecture::from_platform_str(p)
            .map_err(CliInspectError::BadPlatform)?,
        None => umf_core::architecture::Architecture::host(),
    };

    // Pull-on-miss using the same chain `umf run` / `umf build --push` use.
    util::pull_if_missing::<CliInspectError>(
        &layout,
        &reference,
        args.reference,
        args.username,
        args.password_stdin,
        args.insecure_registry,
    )?;

    let report = build_inspect_report(&layout, args.reference, architecture)?;
    match args.format {
        InspectFormat::Table => render_inspect_table(&report, args.show_blobs),
        InspectFormat::Json => {
            let json = serde_json::to_string_pretty(&report)?;
            println!("{json}");
        }
    }
    Ok(())
}

/// Structured inspect report — what we serialise to JSON and use to
/// drive the table renderer. Lives in the CLI (rather than in a
/// library crate) because it's a CLI-shaped view of an OCI image
/// composed from `umf-builder::introspect` + raw manifest + raw
/// image-config reads. Other callers wouldn't reach for this shape.
#[derive(Debug, serde::Serialize)]
struct InspectReport {
    reference: String,
    manifest: ManifestSummary,
    target: TargetSummary,
    image: ImageMetadata,
    runtime: RuntimeConfig,
    layers: Vec<LayerSummary>,
    history: Vec<HistoryEntry>,
    labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, serde::Serialize)]
struct ManifestSummary {
    digest: String,
    schema_version: u8,
    media_type: Option<String>,
    config_digest: String,
    layer_count: usize,
}

#[derive(Debug, serde::Serialize)]
struct TargetSummary {
    kind: String,
    provenance: String,
    spec_version: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
struct ImageMetadata {
    architecture: Option<String>,
    os: Option<String>,
    created: Option<String>,
    author: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
struct RuntimeConfig {
    entrypoint: Vec<String>,
    cmd: Vec<String>,
    env: Vec<String>,
    working_dir: Option<String>,
    user: Option<String>,
    exposed_ports: Vec<String>,
    volumes: Vec<String>,
    stop_signal: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct LayerSummary {
    index: usize,
    digest: String,
    media_type: String,
    size_bytes: i64,
    diff_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct HistoryEntry {
    created: Option<String>,
    created_by: Option<String>,
    author: Option<String>,
    comment: Option<String>,
    empty_layer: bool,
}

fn build_inspect_report(
    layout: &ImageLayout,
    ref_name: &str,
    architecture: umf_core::architecture::Architecture,
) -> Result<InspectReport, CliInspectError> {
    use oci_client::manifest::OciManifest;

    let entry = layout
        .lookup_ref(ref_name)?
        .ok_or_else(|| RegistryError::NotFound(ref_name.to_string()))?;
    let top_bytes = layout.read_blob(&entry.digest)?;
    let parsed: OciManifest = serde_json::from_slice(&top_bytes)?;

    // Resolve to the concrete single-arch manifest + its L0 profile.
    //
    // * Single image: introspect it directly (the established path — label or
    //   manifest-shape inference, unchanged).
    // * Image index: select the child matching the requested `--platform`
    //   arch via the shared `umf-oci` selector (the same preference order the
    //   build / run consume paths use), then derive the profile from that
    //   child's config. Inspect doesn't unpack layers so this stays cheap.
    let (profile, manifest_digest, manifest_bytes) =
        match parsed {
            OciManifest::Image(_) => {
                let profile = umf_builder::introspect::introspect(layout, ref_name)?;
                (profile, entry.digest.clone(), top_bytes)
            }
            OciManifest::ImageIndex(index) => {
                let oci_arch = architecture.oci_arch_string();
                let chosen = umf_oci::image::select_manifest_for_arch(&index, oci_arch)
                    .ok_or_else(|| CliInspectError::NoManifestForPlatform {
                        reference: ref_name.to_string(),
                        arch: oci_arch.to_string(),
                        available: available_platforms(&index),
                    })?;
                let child_bytes = layout.read_blob(&chosen.digest)?;
                let profile = profile_from_child(layout, &chosen.digest, &child_bytes)?;
                (profile, chosen.digest.clone(), child_bytes)
            }
        };
    let manifest: oci_client::manifest::OciImageManifest = serde_json::from_slice(&manifest_bytes)?;
    let config_bytes = layout.read_blob(&manifest.config.digest)?;
    let config: InspectImageConfig = serde_json::from_slice(&config_bytes).unwrap_or_default();

    let layers = manifest
        .layers
        .iter()
        .enumerate()
        .map(|(i, l)| LayerSummary {
            index: i,
            digest: l.digest.clone(),
            media_type: l.media_type.clone(),
            size_bytes: l.size,
            diff_id: config.rootfs.diff_ids.get(i).cloned(),
        })
        .collect();

    let history = config
        .history
        .iter()
        .map(|h| HistoryEntry {
            created: h.created.clone(),
            created_by: h.created_by.clone(),
            author: h.author.clone(),
            comment: h.comment.clone(),
            empty_layer: h.empty_layer.unwrap_or(false),
        })
        .collect();

    let spec_version = profile.labels.get(umf_core::label::SPEC_VERSION).cloned();

    let report = InspectReport {
        reference: ref_name.to_string(),
        manifest: ManifestSummary {
            digest: manifest_digest,
            schema_version: manifest.schema_version,
            media_type: manifest.media_type.clone(),
            config_digest: manifest.config.digest.clone(),
            layer_count: manifest.layers.len(),
        },
        target: TargetSummary {
            kind: format!("{:?}", profile.kind),
            provenance: format!("{:?}", profile.source),
            spec_version,
        },
        image: ImageMetadata {
            architecture: config.architecture.clone(),
            os: config.os.clone(),
            created: config.created.clone(),
            author: config.author.clone(),
        },
        runtime: RuntimeConfig {
            entrypoint: config.config.entrypoint.clone(),
            cmd: config.config.cmd.clone(),
            env: config.config.env.clone(),
            working_dir: nonempty(&config.config.working_dir),
            user: nonempty(&config.config.user),
            exposed_ports: config.config.exposed_ports.keys().cloned().collect(),
            volumes: config.config.volumes.keys().cloned().collect(),
            stop_signal: config.config.stop_signal.clone(),
        },
        layers,
        history,
        labels: profile.labels.clone(),
    };
    Ok(report)
}

fn nonempty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Comma-joined `os/arch[/variant]` list of the platforms an index advertises,
/// for the "no manifest for <arch>" diagnostic.
fn available_platforms(index: &oci_client::manifest::OciImageIndex) -> String {
    let mut seen: Vec<String> = index
        .manifests
        .iter()
        .map(|m| match &m.platform {
            Some(p) => match &p.variant {
                Some(v) if !v.is_empty() => format!("{}/{}/{}", p.os, p.architecture, v),
                _ => format!("{}/{}", p.os, p.architecture),
            },
            None => "(no platform)".to_string(),
        })
        .collect();
    seen.sort();
    seen.dedup();
    if seen.is_empty() {
        "(none)".to_string()
    } else {
        seen.join(", ")
    }
}

/// Derive an [`L0Profile`] for the selected child of an image index.
///
/// `umf_builder::introspect::introspect` only takes a ref name and rejects an
/// index outright, so for the index case we reproduce its logic against the
/// already-selected child manifest + config: read `org.imagilux.umf.type` when
/// present (provenance [`L0Source::Label`]), else infer container-shape from
/// the child manifest structure (a container-like config media type plus at
/// least one layer ⇒ [`L0Kind::Container`], provenance [`L0Source::Inferred`]).
fn profile_from_child(
    layout: &ImageLayout,
    manifest_digest: &str,
    manifest_bytes: &[u8],
) -> Result<umf_builder::introspect::L0Profile, CliInspectError> {
    use oci_client::manifest::{
        IMAGE_CONFIG_MEDIA_TYPE, IMAGE_DOCKER_CONFIG_MEDIA_TYPE, OciImageManifest,
    };
    use umf_core::l0::{L0Kind, L0Source};

    let manifest: OciImageManifest = serde_json::from_slice(manifest_bytes)?;
    let config_bytes = layout.read_blob(&manifest.config.digest)?;
    // Reuse the inspect-config shape but read only Labels off it.
    let doc: InspectImageConfig = serde_json::from_slice(&config_bytes).unwrap_or_default();
    let labels: std::collections::BTreeMap<String, String> = doc.config.labels;

    let (kind, source) = match labels.get(umf_core::label::TYPE) {
        Some(value) => (L0Kind::from_label(value), L0Source::Label),
        None => {
            let container_like = matches!(
                manifest.config.media_type.as_str(),
                IMAGE_CONFIG_MEDIA_TYPE | IMAGE_DOCKER_CONFIG_MEDIA_TYPE,
            );
            let kind = if container_like && !manifest.layers.is_empty() {
                L0Kind::Container
            } else {
                L0Kind::Unknown(String::new())
            };
            (kind, L0Source::Inferred)
        }
    };

    Ok(umf_builder::introspect::L0Profile {
        kind,
        source,
        manifest_digest: manifest_digest.to_string(),
        labels,
    })
}

/// Local mirror of the OCI image-config JSON shape — richer than what
/// `umf-engine::bundle` reads (we need user / exposed-ports / volumes /
/// stop-signal / history / labels for the inspect view).
#[derive(Debug, Default, serde::Deserialize)]
struct InspectImageConfig {
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    config: InspectContainerConfig,
    #[serde(default)]
    rootfs: InspectRootfs,
    #[serde(default)]
    history: Vec<InspectHistoryEntry>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct InspectContainerConfig {
    #[serde(default, rename = "User")]
    user: String,
    #[serde(default, rename = "Env")]
    env: Vec<String>,
    #[serde(default, rename = "Entrypoint")]
    entrypoint: Vec<String>,
    #[serde(default, rename = "Cmd")]
    cmd: Vec<String>,
    #[serde(default, rename = "WorkingDir")]
    working_dir: String,
    #[serde(default, rename = "ExposedPorts")]
    exposed_ports: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "Volumes")]
    volumes: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "StopSignal")]
    stop_signal: Option<String>,
    /// OCI config `Labels`. Read for the index-child path's L0 profile (the
    /// table/JSON `labels` block otherwise comes from
    /// `umf_builder::introspect`).
    #[serde(default, rename = "Labels")]
    labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct InspectRootfs {
    #[serde(default)]
    diff_ids: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct InspectHistoryEntry {
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    empty_layer: Option<bool>,
}

fn render_inspect_table(r: &InspectReport, show_blobs: bool) {
    println!("Reference: {}", r.reference);
    println!("Manifest:  {}", r.manifest.digest);
    println!(
        "Schema:    v{} ({})",
        r.manifest.schema_version,
        r.manifest.media_type.as_deref().unwrap_or("unknown"),
    );
    println!();
    println!(
        "Target type:  {} (from {})",
        r.target.kind, r.target.provenance
    );
    if let Some(spec) = &r.target.spec_version {
        println!("Spec version: {spec}");
    }
    println!();
    println!(
        "Architecture: {}",
        r.image.architecture.as_deref().unwrap_or("(unknown)"),
    );
    println!(
        "OS:           {}",
        r.image.os.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "Created:      {}",
        r.image.created.as_deref().unwrap_or("(unset)")
    );
    println!(
        "Author:       {}",
        r.image.author.as_deref().unwrap_or("(unset)")
    );
    println!();
    println!("Runtime config:");
    println!(
        "  ENTRYPOINT  {}",
        if r.runtime.entrypoint.is_empty() {
            "(none)".to_string()
        } else {
            r.runtime.entrypoint.join(" ")
        },
    );
    println!(
        "  CMD         {}",
        if r.runtime.cmd.is_empty() {
            "(none)".to_string()
        } else {
            r.runtime.cmd.join(" ")
        },
    );
    if r.runtime.env.is_empty() {
        println!("  ENV         (none)");
    } else {
        println!("  ENV         {}", r.runtime.env[0]);
        for entry in &r.runtime.env[1..] {
            println!("              {entry}");
        }
    }
    println!(
        "  WORKDIR     {}",
        r.runtime.working_dir.as_deref().unwrap_or("(default)"),
    );
    println!(
        "  USER        {}",
        r.runtime.user.as_deref().unwrap_or("(default)")
    );
    println!(
        "  EXPOSE      {}",
        if r.runtime.exposed_ports.is_empty() {
            "(none)".to_string()
        } else {
            r.runtime.exposed_ports.join(", ")
        },
    );
    println!(
        "  VOLUME      {}",
        if r.runtime.volumes.is_empty() {
            "(none)".to_string()
        } else {
            r.runtime.volumes.join(", ")
        },
    );
    println!(
        "  STOPSIGNAL  {}",
        r.runtime.stop_signal.as_deref().unwrap_or("(default)"),
    );
    println!();
    println!("Layers ({}):", r.layers.len());
    for layer in &r.layers {
        if show_blobs {
            println!(
                "  [{:>2}] {} ({}B {})  diff: {}",
                layer.index,
                layer.digest,
                util::human_size(layer.size_bytes),
                short_media(&layer.media_type),
                layer.diff_id.as_deref().unwrap_or("(unknown)"),
            );
        } else {
            println!(
                "  [{:>2}] {}  {}B {}",
                layer.index,
                util::truncate_chars(&layer.digest, 19),
                util::human_size(layer.size_bytes),
                short_media(&layer.media_type),
            );
        }
    }
    println!();
    if !r.history.is_empty() {
        println!("History ({}):", r.history.len());
        for (i, h) in r.history.iter().enumerate() {
            let marker = if h.empty_layer { "·" } else { "✓" };
            println!(
                "  [{:>2}] {} {} {}",
                i,
                marker,
                h.created.as_deref().unwrap_or("(no time)"),
                h.created_by.as_deref().unwrap_or("(no command)"),
            );
        }
        println!();
    }
    println!("Labels ({}):", r.labels.len());
    let mut umf_labels: Vec<_> = r
        .labels
        .iter()
        .filter(|(k, _)| k.starts_with("org.imagilux.umf."))
        .collect();
    let mut other_labels: Vec<_> = r
        .labels
        .iter()
        .filter(|(k, _)| !k.starts_with("org.imagilux.umf."))
        .collect();
    umf_labels.sort_by(|a, b| a.0.cmp(b.0));
    other_labels.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in umf_labels.iter().chain(other_labels.iter()) {
        println!("  {k} = {v}");
    }
}

fn short_media(m: &str) -> &str {
    // Trim the noisy `application/vnd.oci.image.layer.v1.tar+gzip`
    // shape to just the last segment for the compact view.
    m.rsplit('.').next().unwrap_or(m)
}
