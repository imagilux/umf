//! FROM-image pull + base-image resolution.
//!
//! Pulls the FROM reference into the layout (skipping the network
//! round-trip when it's already cached), then reads the base image's
//! manifest + image-config to seed the in-progress [`ImageConfig`] and
//! the base layer chain.

use std::collections::BTreeMap;

use bytes::Bytes;
use oci_client::manifest::OciImageManifest;
use tracing::debug;
use umf_core::architecture::Architecture;
use umf_core::l0::L0Kind;
use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource};
use umf_oci::registry::error::RegistryError;
use umf_oci::registry::{
    CredentialOverride, ImageLayout, Reference, RegistryClient, SearchRegistries,
    resolution_candidates, resolve_auth_for,
};

use super::EngineBuildError;
use super::state::BaseImage;

/// Pull `ref_name` into `layout` and return the canonical (registry-qualified,
/// repository-canonicalized) form under which the pull was recorded —
/// callers should use *that* form when looking the ref up in the layout
/// (which is what the registry client stored).
///
/// Skips the network round-trip if the layout already contains the ref —
/// callers building the same recipe twice (or building a recipe whose
/// FROM is also in their local layout from a separate build) don't pay
/// the registry round-trip on every invocation. The FROM image is
/// content-addressed by digest; refreshing a moving tag is the caller's
/// responsibility (pull explicitly with the registry client).
pub(crate) async fn pull_into_layout(
    layout: &ImageLayout,
    ref_name: &str,
) -> Result<String, EngineBuildError> {
    // Validate the reference up front so a genuinely malformed `FROM` reports a
    // clear error rather than a chain-exhausted miss.
    let _: Reference = ref_name.parse().map_err(|e: oci_client::ParseError| {
        EngineBuildError::InvalidReference(ref_name.to_string(), e.to_string())
    })?;

    // Expand an *unqualified* reference against the operator's search registries
    // (then the docker.io default). A qualified ref, or an empty search list,
    // yields just itself, so behaviour is unchanged unless registries are
    // configured via `umf registry`.
    let search = SearchRegistries::load().search;
    let candidates = resolution_candidates(ref_name, &search);
    let client = RegistryClient::new();
    let mut last_err: Option<EngineBuildError> = None;

    for candidate in &candidates {
        let Ok(reference) = candidate.parse::<Reference>() else {
            continue;
        };
        let canonical = reference.whole();
        if layout.lookup_ref(&canonical)?.is_some() {
            debug!(ref_name = %canonical, "FROM image already in layout; skipping pull");
            return Ok(canonical);
        }
        // Pick up creds per candidate host (env / `~/.docker/config.json`),
        // falling back to anonymous when nothing is configured.
        let auth = resolve_auth_for(Some(reference.registry()), &CredentialOverride::default());
        match client.pull(&reference, &auth, layout).await {
            Ok(_) => return Ok(canonical),
            Err(e) => {
                if candidates.len() > 1 {
                    debug!(candidate = %canonical, error = %e, "FROM candidate pull failed; trying next");
                }
                last_err = Some(e.into());
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        EngineBuildError::InvalidReference(ref_name.to_string(), "no resolvable candidate".into())
    }))
}

/// The `FROM scratch` base: no layers, an empty config stamped with the
/// target architecture. No image is read — `scratch` is a keyword, not a
/// reference.
pub(crate) fn scratch_base_image(architecture: Architecture) -> BaseImage {
    BaseImage {
        layers: Vec::new(),
        config: ImageConfig {
            architecture: architecture.oci_arch_string().to_string(),
            ..ImageConfig::default()
        },
    }
}

pub(crate) fn resolve_base_image(
    layout: &ImageLayout,
    ref_name: &str,
    architecture: Architecture,
) -> Result<BaseImage, EngineBuildError> {
    use oci_client::manifest::OciManifest;
    use oci_spec::image::{Arch, Os};

    // The target arch as an OCI `Arch`. `oci_arch_string` yields the OCI
    // shorthand (`amd64` / `arm64`), which `Arch::from(&str)` maps to the
    // matching variant, so adding an architecture stays a single
    // `umf_core::Architecture` change.
    let want_arch = Arch::from(architecture.oci_arch_string());

    let entry = layout
        .lookup_ref(ref_name)?
        .ok_or_else(|| RegistryError::NotFound(ref_name.to_string()))?;
    let manifest_bytes = layout.read_blob(&entry.digest)?;
    let manifest: OciImageManifest = match serde_json::from_slice::<OciManifest>(&manifest_bytes)? {
        OciManifest::Image(image) => image,
        OciManifest::ImageIndex(index) => {
            // Select the manifest whose platform matches the requested
            // `--platform` arch. Prefer the variant-less match, then any
            // variant of the same arch. No match is an error: silently
            // falling back to the first manifest would pull the wrong arch.
            let chosen = index
                .manifests
                .iter()
                .find(|m| {
                    m.platform.as_ref().is_some_and(|p| {
                        p.os == Os::Linux
                            && p.architecture == want_arch
                            && p.variant.as_deref().is_none_or(|v| v.is_empty())
                    })
                })
                .or_else(|| {
                    index.manifests.iter().find(|m| {
                        m.platform
                            .as_ref()
                            .is_some_and(|p| p.os == Os::Linux && p.architecture == want_arch)
                    })
                })
                .ok_or_else(|| EngineBuildError::NoManifestForPlatform {
                    arch: architecture.oci_arch_string().to_string(),
                })?;
            let child_bytes = layout.read_blob(&chosen.digest)?;
            serde_json::from_slice(&child_bytes)?
        }
    };

    let config_bytes = layout.read_blob(&manifest.config.digest)?;
    let raw_config: serde_json::Value = serde_json::from_slice(&config_bytes)?;
    let image_config = image_config_from_raw(&raw_config, architecture);

    let diff_ids = extract_diff_ids(&raw_config);
    if diff_ids.len() != manifest.layers.len() {
        return Err(EngineBuildError::BaseImageLayerCountMismatch {
            layers: manifest.layers.len(),
            diff_ids: diff_ids.len(),
        });
    }

    let mut layers = Vec::with_capacity(manifest.layers.len());
    for (descriptor, diff_id) in manifest.layers.iter().zip(diff_ids.iter()) {
        let blob = layout.read_blob(&descriptor.digest)?;
        layers.push(LayerSource {
            data: Bytes::from(blob),
            media_type: descriptor.media_type.clone(),
            diff_id: diff_id.clone(),
        });
    }

    Ok(BaseImage {
        layers,
        config: image_config,
    })
}

/// Translate a raw image-config JSON into our [`ImageConfig`].
///
/// The emitted image's `architecture` is the *target* arch (the
/// `--platform` we built for), not whatever the base image's config
/// happened to record. They agree for a matched manifest, but stamping
/// the target keeps the field authoritative even for a single-platform
/// base.
fn image_config_from_raw(raw: &serde_json::Value, architecture: Architecture) -> ImageConfig {
    let architecture = architecture.oci_arch_string().to_string();
    let os = raw
        .get("os")
        .and_then(|v| v.as_str())
        .unwrap_or("linux")
        .to_string();

    let mut container = ContainerConfig::default();
    if let Some(cfg) = raw.get("config") {
        if let Some(env) = cfg.get("Env").and_then(|v| v.as_array()) {
            container.env = env
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(entrypoint) = cfg.get("Entrypoint").and_then(|v| v.as_array()) {
            let argv: Vec<String> = entrypoint
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !argv.is_empty() {
                container.entrypoint = Some(argv);
            }
        }
        if let Some(cmd) = cfg.get("Cmd").and_then(|v| v.as_array()) {
            let argv: Vec<String> = cmd
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !argv.is_empty() {
                container.cmd = Some(argv);
            }
        }
        if let Some(wd) = cfg.get("WorkingDir").and_then(|v| v.as_str())
            && !wd.is_empty()
        {
            container.working_dir = Some(wd.to_string());
        }
        if let Some(user) = cfg.get("User").and_then(|v| v.as_str())
            && !user.is_empty()
        {
            container.user = Some(user.to_string());
        }
        if let Some(labels) = cfg.get("Labels").and_then(|v| v.as_object()) {
            let mut out: BTreeMap<String, String> = BTreeMap::new();
            for (k, v) in labels {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
            container.labels = out;
        }
    }

    ImageConfig {
        architecture,
        os,
        created: None,
        author: None,
        container,
        history: Vec::new(),
        umf_type: L0Kind::Container,
        umf_spec: None,
    }
}

fn extract_diff_ids(raw: &serde_json::Value) -> Vec<String> {
    raw.get("rootfs")
        .and_then(|r| r.get("diff_ids"))
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
