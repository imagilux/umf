//! Unit tests for the referrers wire types and the fallback tag schema.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

fn descriptor(digest: &str, artifact_type: Option<&str>) -> ReferrerDescriptor {
    ReferrerDescriptor {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        digest: digest.to_string(),
        size: 42,
        artifact_type: artifact_type.map(str::to_string),
        annotations: None,
    }
}

#[test]
fn fallback_tag_swaps_the_colon() {
    let tag =
        fallback_tag("sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a")
            .expect("well-formed digest");
    assert_eq!(
        tag,
        "sha256-44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

#[test]
fn fallback_tag_truncates_to_the_tag_limit() {
    let digest = format!("sha512:{}", "ab".repeat(64));
    let tag = fallback_tag(&digest).expect("well-formed sha512 digest");
    assert_eq!(tag.len(), 127);
    assert!(tag.starts_with("sha512-abab"));
}

#[test]
fn fallback_tag_rejects_malformed_digests() {
    for bad in ["", "sha256", "sha256:", ":abcd", "sha256:zz", "SHA256:abcd"] {
        let err = fallback_tag(bad).expect_err("malformed digest must be rejected");
        assert!(
            matches!(err, RegistryError::MalformedDigest(_)),
            "unexpected error for {bad:?}: {err}",
        );
    }
}

#[test]
fn upsert_replaces_by_digest_and_keeps_digest_order() {
    let mut index = ReferrersIndex::empty();
    index.upsert(descriptor("sha256:bbbb", Some("application/spdx+json")));
    index.upsert(descriptor("sha256:aaaa", None));
    // Same digest again — replaces, no duplicate.
    index.upsert(descriptor(
        "sha256:bbbb",
        Some("application/vnd.example.sig"),
    ));

    let digests: Vec<&str> = index.manifests.iter().map(|d| d.digest.as_str()).collect();
    assert_eq!(digests, ["sha256:aaaa", "sha256:bbbb"]);
    assert_eq!(
        index.manifests[1].artifact_type.as_deref(),
        Some("application/vnd.example.sig"),
        "upsert replaces the existing descriptor",
    );
}

#[test]
fn filtered_drops_non_matching_artifact_types() {
    let mut index = ReferrersIndex::empty();
    index.upsert(descriptor("sha256:aaaa", Some("application/spdx+json")));
    index.upsert(descriptor(
        "sha256:bbbb",
        Some("application/vnd.example.sig"),
    ));
    index.upsert(descriptor("sha256:cccc", None));

    assert_eq!(index.clone().filtered(None).len(), 3);
    let only_sbom = index.filtered(Some("application/spdx+json"));
    assert_eq!(only_sbom.len(), 1);
    assert_eq!(only_sbom[0].digest, "sha256:aaaa");
}

#[test]
fn index_serde_uses_the_wire_field_names() {
    let mut index = ReferrersIndex::empty();
    index.upsert(descriptor("sha256:aaaa", Some("application/spdx+json")));

    let value = serde_json::to_value(&index).expect("serialize");
    assert_eq!(value["schemaVersion"], 2);
    assert_eq!(value["mediaType"], OCI_IMAGE_INDEX_MEDIA_TYPE);
    assert_eq!(
        value["manifests"][0]["artifactType"],
        "application/spdx+json"
    );
    assert_eq!(
        value["manifests"][0]["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );

    // Round-trip, tolerating fields this client doesn't model.
    let mut wire = value;
    wire["annotations"] = serde_json::json!({"org.example": "1"});
    let parsed: ReferrersIndex = serde_json::from_value(wire).expect("parse");
    assert_eq!(parsed, index);
}
