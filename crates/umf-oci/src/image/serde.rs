//! Private OCI-JSON wire structs for image-config emission.
//!
//! These mirror the on-disk OCI image config JSON document. They are an
//! implementation detail of [`super::emit_image`]: the public
//! [`super::ImageConfig`] / [`super::ContainerConfig`] shapes stay ergonomic
//! (e.g. `Vec<String>` for exposed ports) and are converted into these spec
//! shapes (`BTreeMap<String, EmptyObject>`) at emission time.

use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub(super) struct OciImageConfigDoc {
    pub(super) architecture: String,
    pub(super) os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) author: Option<String>,
    pub(super) config: SerializedContainerConfig,
    pub(super) rootfs: Rootfs,
    /// Per-layer / per-step history. Omitted from the serialised JSON when
    /// empty so producers that don't track history don't write an empty
    /// array into every config blob.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) history: Vec<SerializedHistoryEntry>,
}

/// On-wire shape of a single `history` entry.
///
/// `empty_layer` is `Option<bool>` because the OCI spec defines its default
/// as `false` — omitting it instead of writing `false` is the conventional
/// producer behaviour and keeps trivial config blobs minimal.
#[derive(Debug, Serialize)]
pub(super) struct SerializedHistoryEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) empty_layer: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct SerializedContainerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) user: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) entrypoint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cmd: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) working_dir: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) exposed_ports: BTreeMap<String, EmptyObject>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) volumes: BTreeMap<String, EmptyObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stop_signal: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) labels: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub(super) struct EmptyObject {}

#[derive(Debug, Serialize)]
pub(super) struct Rootfs {
    #[serde(rename = "type")]
    pub(super) fs_type: String,
    pub(super) diff_ids: Vec<String>,
}
