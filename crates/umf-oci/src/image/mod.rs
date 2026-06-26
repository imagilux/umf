//! OCI image emission.
//!
//! The producer-side counterpart to [`crate::registry::RegistryClient::pull`]:
//! given layer payloads plus a config, assemble a valid OCI image (manifest +
//! config blob + layer blobs) into an [`crate::registry::ImageLayout`]. The
//! emitted image is immediately consumable by
//! [`crate::registry::RegistryClient::push`] and by external tooling
//! (`crane`/`skopeo`) reading the same on-disk layout.
//!
//! Layer-from-RUN-step diffing is **not** in this module — that lives with the
//! RUN backend. [`emit_image`] takes ready-made layers.
//!
//! ## UMF label injection
//!
//! Two labels are always written into the image config's `Labels` map, on top
//! of whatever the caller supplied (caller-supplied entries with the same keys
//! are overridden):
//!
//! * [`umf_core::label::TYPE`] — value from [`ImageConfig::umf_type`], rendered
//!   via [`L0Kind`]'s `Display` impl.
//! * [`umf_core::label::SPEC_VERSION`] — value from
//!   [`ImageConfig::umf_spec`], defaulting to
//!   [`umf_core::label::CURRENT_SPEC_VERSION`].
//!
//! ## Reproducibility
//!
//! The acceptance criterion is "byte-for-byte given the same
//! inputs". Three things make that hold:
//!
//! * `BTreeMap<String, _>` everywhere — labels, exposed ports, volumes — for
//!   stable serialisation order.
//! * No implicit timestamps. `ImageConfig::created` is optional and the caller
//!   chooses whether to set it.
//! * [`LayerSource::from_directory`] walks the tree in lexicographic order and
//!   uses [`tar::HeaderMode::Deterministic`] so two calls with the same
//!   directory contents produce byte-identical tar (and therefore identical
//!   diff_ids).

mod artifact;
mod index;
mod serde;
mod tar_util;

pub use artifact::{
    ArtifactBlob, EMPTY_JSON_MEDIA_TYPE, emit_artifact_manifest, subject_from_entry,
};
pub use index::{IndexChild, emit_index, platform_for, select_manifest_for_arch};

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use bytes::Bytes;
use flate2::Compression as GzCompression;
use flate2::write::GzEncoder;
use oci_client::manifest::{
    IMAGE_CONFIG_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_MEDIA_TYPE,
    OciDescriptor, OciImageManifest,
};
use umf_core::l0::L0Kind;
use umf_core::label;

use crate::registry::ImageLayout;
use crate::registry::error::RegistryError;
use crate::registry::layout::sha256_digest;

use self::serde::{
    EmptyObject, OciImageConfigDoc, Rootfs, SerializedContainerConfig, SerializedHistoryEntry,
};
use self::tar_util::build_tar;

/// OCI image-spec media type for a zstd-compressed tar layer.
///
/// `oci-client` 0.16 exports the gzip and plain-tar layer constants but not
/// the zstd one, so it is spelled out here (image-spec `layer.md`).
pub const IMAGE_LAYER_ZSTD_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// Fixed zstd compression level for emitted layers.
///
/// Pinned (rather than zstd's `0` = "use the library default") so the encoded
/// blob bytes stay stable for a given `zstd` crate version: byte
/// reproducibility is a hard invariant, and a default that shifts between
/// libzstd releases would silently change the blob digest. Level 3 is zstd's
/// own default and a good size/speed balance for image layers.
const ZSTD_LEVEL: i32 = 3;

// ── Public types ────────────────────────────────────────────────────────────

/// Compression codec for an emitted layer blob.
///
/// The codec changes only the *blob* (its bytes, digest, and media type); the
/// layer's `diff_id` is always the sha256 of the **uncompressed** tar and is
/// therefore identical across codecs for the same directory contents.
///
/// [`Self::Gzip`] is the default ([`Default`]) so existing emission paths and
/// their byte-for-byte reproducibility expectations are unaffected; opt into
/// [`Self::Zstd`] explicitly via [`LayerSource::from_directory_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerCompression {
    /// gzip (`application/vnd.oci.image.layer.v1.tar+gzip`). The default.
    #[default]
    Gzip,
    /// zstandard (`application/vnd.oci.image.layer.v1.tar+zstd`).
    Zstd,
}

impl LayerCompression {
    /// OCI media type for a layer blob compressed with this codec.
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Gzip => IMAGE_LAYER_GZIP_MEDIA_TYPE,
            Self::Zstd => IMAGE_LAYER_ZSTD_MEDIA_TYPE,
        }
    }
}

/// One layer of an image being emitted.
///
/// Carries the *compressed* tar bytes that become the blob written to the
/// layout, plus the **diff_id** — the sha256 of the *uncompressed* tar. OCI's
/// `rootfs.diff_ids` references diff_ids, not blob digests, so both are needed
/// and they differ. The compression codec ([`LayerCompression`]) is reflected
/// in [`Self::media_type`] but never in [`Self::diff_id`].
#[derive(Debug, Clone)]
pub struct LayerSource {
    /// Compressed tar bytes — what gets written into `blobs/sha256/<digest>`.
    pub data: Bytes,
    /// Media type for this layer's manifest descriptor.
    pub media_type: String,
    /// sha256 of the *uncompressed* tar (the form referenced by
    /// `rootfs.diff_ids` in the image config).
    pub diff_id: String,
}

impl LayerSource {
    /// Tar + gzip a directory tree into a layer.
    ///
    /// Uses [`tar::HeaderMode::Deterministic`] (zero owners, zero mtimes) and
    /// walks the directory in lexicographic order, so two calls with the same
    /// directory contents produce byte-identical layers (and therefore the
    /// same `diff_id` and blob digest).
    ///
    /// Equivalent to [`Self::from_directory_with`] with
    /// [`LayerCompression::Gzip`]; gzip is the default codec.
    pub fn from_directory(root: &Path) -> Result<Self, RegistryError> {
        Self::from_directory_with(root, LayerCompression::Gzip)
    }

    /// Tar a directory tree and compress it with `compression` into a layer.
    ///
    /// The deterministic tar (see [`Self::from_directory`]) is shared across
    /// codecs, so the resulting `diff_id` is identical regardless of
    /// `compression`; only the blob bytes, blob digest, and `media_type`
    /// differ. Each codec is itself deterministic for a fixed crate version,
    /// so two calls with the same directory + codec produce byte-identical
    /// blobs.
    pub fn from_directory_with(
        root: &Path,
        compression: LayerCompression,
    ) -> Result<Self, RegistryError> {
        let tar_bytes = build_tar(root)?;
        let diff_id = sha256_digest(&tar_bytes);
        let data = compress(&tar_bytes, compression)?;

        Ok(Self {
            data: Bytes::from(data),
            media_type: compression.media_type().to_string(),
            diff_id,
        })
    }
}

/// Compress an uncompressed tar with `compression`, returning the blob bytes.
fn compress(tar_bytes: &[u8], compression: LayerCompression) -> Result<Vec<u8>, RegistryError> {
    let mut buf = Vec::with_capacity(tar_bytes.len() / 2);
    match compression {
        LayerCompression::Gzip => {
            let mut encoder = GzEncoder::new(&mut buf, GzCompression::default());
            encoder.write_all(tar_bytes)?;
            encoder.finish()?;
        }
        LayerCompression::Zstd => {
            let mut encoder = zstd::stream::write::Encoder::new(&mut buf, ZSTD_LEVEL)?;
            encoder.write_all(tar_bytes)?;
            encoder.finish()?;
        }
    }
    Ok(buf)
}

/// User-supplied configuration for an emitted image.
///
/// Maps to the OCI image config JSON document; the public field shape stays
/// ergonomic (e.g. `Vec<String>` for exposed ports rather than the spec's
/// `BTreeMap<String, EmptyObject>`) and the conversion to the spec shape
/// happens inside [`emit_image`].
#[derive(Debug, Clone)]
pub struct ImageConfig {
    /// Target architecture (`"amd64"`, `"arm64"`, …).
    pub architecture: String,
    /// Target operating system, typically `"linux"`.
    pub os: String,
    /// Optional RFC3339 timestamp. Leave `None` for reproducible builds.
    pub created: Option<String>,
    /// Optional author string.
    pub author: Option<String>,
    /// Runtime configuration (env / entrypoint / cmd / labels / …).
    pub container: ContainerConfig,
    /// Per-layer history. When the count of non-`empty_layer` entries equals
    /// `rootfs.diff_ids.len()` (a soft OCI image-spec requirement), registry
    /// UIs that read `history[i].created_by` can display the build step
    /// responsible for each layer.
    ///
    /// Default: empty. Container builds populate this from the upstream image
    /// the runner produced (see the config-merge path in the stage builder);
    /// other producers may leave it empty.
    pub history: Vec<HistoryEntry>,
    /// UMF artifact shape — injected as `org.imagilux.umf.type`. Required.
    pub umf_type: L0Kind,
    /// UMF spec version — injected as `org.imagilux.umf.spec`. Defaults to
    /// [`label::CURRENT_SPEC_VERSION`] when `None`.
    pub umf_spec: Option<String>,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            architecture: "amd64".to_string(),
            os: "linux".to_string(),
            created: None,
            author: None,
            container: ContainerConfig::default(),
            history: Vec::new(),
            umf_type: L0Kind::Container,
            umf_spec: None,
        }
    }
}

/// One entry in an OCI image config's `history` array.
///
/// Each entry corresponds to one build step. Entries with
/// `empty_layer = true` describe steps that didn't produce a filesystem
/// change (e.g. `LABEL`, `ENV`, `CMD`) and are not paired with a layer in
/// `rootfs.diff_ids`. Entries with `empty_layer = false` (or absent) are
/// paired with a layer in source-declaration order.
#[derive(Debug, Clone, Default)]
pub struct HistoryEntry {
    /// RFC 3339 timestamp of when this step ran.
    pub created: Option<String>,
    /// Free-form description of how the step was produced. Typical content
    /// is the recipe instruction text, sometimes with a `/bin/sh -c` prefix
    /// when the build runner shelled the step out.
    pub created_by: Option<String>,
    /// Author string carried over from the upstream build.
    pub author: Option<String>,
    /// Free-form comment.
    pub comment: Option<String>,
    /// `true` when this step produced no filesystem change.
    pub empty_layer: bool,
}

/// Runtime configuration — the OCI image config's `config` sub-object.
#[derive(Debug, Default, Clone)]
pub struct ContainerConfig {
    /// `USER` directive equivalent.
    pub user: Option<String>,
    /// Environment variables in `KEY=VAL` form.
    pub env: Vec<String>,
    /// `ENTRYPOINT` exec form.
    pub entrypoint: Option<Vec<String>>,
    /// `CMD` exec form.
    pub cmd: Option<Vec<String>>,
    /// `WORKDIR` directive equivalent.
    pub working_dir: Option<String>,
    /// `EXPOSE` directive — entries in `"port/proto"` form (`"80/tcp"`).
    pub exposed_ports: Vec<String>,
    /// `VOLUME` directive — absolute paths.
    pub volumes: Vec<String>,
    /// Signal used to gracefully stop the process.
    pub stop_signal: Option<String>,
    /// Image labels. UMF labels ([`label::TYPE`], [`label::SPEC_VERSION`]) are
    /// injected on top of these by [`emit_image`].
    pub labels: BTreeMap<String, String>,
}

/// Assemble an OCI image from `layers` + `config`, write all blobs and the
/// manifest into `layout`, and register the manifest under `ref_name`.
///
/// Returns the descriptor recorded in `index.json` — same shape as the entry
/// [`crate::registry::RegistryClient::pull`] writes. After this returns, the
/// image is immediately consumable by `RegistryClient::push` or by external
/// tooling (`crane push`, `skopeo copy`).
pub fn emit_image(
    layout: &ImageLayout,
    layers: &[LayerSource],
    config: &ImageConfig,
    ref_name: &str,
) -> Result<ImageIndexEntry, RegistryError> {
    // 1. Layer blobs.
    let mut layer_descriptors = Vec::with_capacity(layers.len());
    let mut diff_ids = Vec::with_capacity(layers.len());
    for layer in layers {
        let blob_digest = sha256_digest(&layer.data);
        layout.write_blob_with_digest(&layer.data, &blob_digest)?;
        layer_descriptors.push(OciDescriptor {
            media_type: layer.media_type.clone(),
            digest: blob_digest,
            size: layer.data.len() as i64,
            urls: None,
            annotations: None,
        });
        diff_ids.push(layer.diff_id.clone());
    }

    // 2. Image config blob — UMF labels injected on top of caller labels.
    let mut final_labels = config.container.labels.clone();
    final_labels.insert(label::TYPE.to_string(), config.umf_type.to_string());
    let spec_version = config
        .umf_spec
        .clone()
        .unwrap_or_else(|| label::CURRENT_SPEC_VERSION.to_string());
    final_labels.insert(label::SPEC_VERSION.to_string(), spec_version);

    let config_doc = OciImageConfigDoc {
        architecture: config.architecture.clone(),
        os: config.os.clone(),
        created: config.created.clone(),
        author: config.author.clone(),
        config: SerializedContainerConfig {
            user: config.container.user.clone(),
            env: config.container.env.clone(),
            entrypoint: config.container.entrypoint.clone(),
            cmd: config.container.cmd.clone(),
            working_dir: config.container.working_dir.clone(),
            exposed_ports: config
                .container
                .exposed_ports
                .iter()
                .map(|p| (p.clone(), EmptyObject {}))
                .collect(),
            volumes: config
                .container
                .volumes
                .iter()
                .map(|v| (v.clone(), EmptyObject {}))
                .collect(),
            stop_signal: config.container.stop_signal.clone(),
            labels: final_labels,
        },
        rootfs: Rootfs {
            fs_type: "layers".to_string(),
            diff_ids,
        },
        history: config
            .history
            .iter()
            .map(|h| SerializedHistoryEntry {
                created: h.created.clone(),
                created_by: h.created_by.clone(),
                author: h.author.clone(),
                comment: h.comment.clone(),
                empty_layer: if h.empty_layer { Some(true) } else { None },
            })
            .collect(),
    };
    let config_bytes = serde_json::to_vec(&config_doc)?;
    let config_digest = layout.write_blob(&config_bytes)?;

    // 3. Manifest.
    let manifest = OciImageManifest {
        schema_version: 2,
        media_type: Some(OCI_IMAGE_MEDIA_TYPE.to_string()),
        config: OciDescriptor {
            media_type: IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            digest: config_digest,
            size: config_bytes.len() as i64,
            urls: None,
            annotations: None,
        },
        layers: layer_descriptors,
        subject: None,
        artifact_type: None,
        annotations: None,
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = sha256_digest(&manifest_bytes);
    layout.write_blob_with_digest(&manifest_bytes, &manifest_digest)?;

    // 4. Register in index.json under the ref name.
    let entry = ImageIndexEntry {
        media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
        digest: manifest_digest,
        size: manifest_bytes.len() as i64,
        platform: None,
        annotations: None,
    };
    layout.upsert_ref(ref_name, entry.clone())?;
    Ok(entry)
}

#[cfg(test)]
mod tests;
