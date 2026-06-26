//! Unit tests for the `client` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn registry_client_is_constructible() {
    let _c = RegistryClient::new();
}

#[test]
fn blob_byte_cap_clamps_declared_size() {
    // Bogus / absent declared sizes fall back to the hard ceiling.
    assert_eq!(blob_byte_cap(-1), MAX_BLOB_BYTES);
    assert_eq!(blob_byte_cap(0), MAX_BLOB_BYTES);
    assert_eq!(blob_byte_cap(i64::MAX), MAX_BLOB_BYTES);
    // A sane declared size is honoured as the ceiling.
    assert_eq!(blob_byte_cap(4096), 4096);
}

#[tokio::test]
async fn capped_buf_rejects_overflow() {
    use tokio::io::AsyncWriteExt as _;
    let mut sink = CappedBuf::with_cap(4);
    sink.write_all(b"ok").await.expect("under cap");
    // A registry that streams past the declared ceiling is rejected before
    // the buffer grows further — this is the DoS guard.
    let err = sink.write_all(b"toolong").await.expect_err("over cap");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(sink.into_inner().len() <= 4);
}

#[test]
fn accepted_media_types_cover_oci_and_docker() {
    assert!(ACCEPTED_MANIFEST_MEDIA_TYPES.contains(&OCI_IMAGE_MEDIA_TYPE));
    assert!(ACCEPTED_MANIFEST_MEDIA_TYPES.contains(&OCI_IMAGE_INDEX_MEDIA_TYPE));
    assert!(ACCEPTED_MANIFEST_MEDIA_TYPES.contains(&IMAGE_MANIFEST_MEDIA_TYPE));
    assert!(ACCEPTED_MANIFEST_MEDIA_TYPES.contains(&IMAGE_MANIFEST_LIST_MEDIA_TYPE));
}

#[test]
fn ref_annotated_entry_sets_annotation() {
    let base = ImageIndexEntry {
        media_type: OCI_IMAGE_MEDIA_TYPE.to_string(),
        digest: "sha256:cafe".into(),
        size: 0,
        platform: None,
        annotations: None,
    };
    let annotated = ref_annotated_entry("foo:bar", base);
    let annotations = annotated.annotations.expect("annotations set");
    assert_eq!(
        annotations.get(REF_NAME_ANNOTATION).map(String::as_str),
        Some("foo:bar"),
    );
}

#[test]
fn default_client_config_sets_connect_and_read_timeouts() {
    // A bare `ClientConfig::default()` leaves both timeouts `None`, which is
    // what lets a stalled or black-holed registry hang the CLI forever. Ours
    // must set both ceilings (#C audit finding).
    let cfg = default_client_config();
    assert!(cfg.read_timeout.is_some(), "read timeout must be set");
    assert!(cfg.connect_timeout.is_some(), "connect timeout must be set");
}

#[test]
fn is_transient_classifies_transport_vs_permanent_errors() {
    use std::io;
    // Transport / IO blips are retryable.
    assert!(
        RegistryError::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset")).is_transient()
    );
    // Local / content / permanent failures are not: a retry only repeats them.
    assert!(!RegistryError::NotFound("ref".into()).is_transient());
    assert!(!RegistryError::MalformedDigest("bad".into()).is_transient());
    assert!(
        !RegistryError::DigestMismatch {
            expected: "a".into(),
            found: "b".into(),
        }
        .is_transient()
    );
}

#[test]
fn retry_backoff_grows_then_exhausts_for_transient_errors() {
    use std::io;
    let transient = RegistryError::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
    // Attempts 1 and 2 retry with a doubling backoff; the MAX'th attempt gives
    // up (returns None) so the loop terminates instead of retrying forever.
    assert_eq!(retry_backoff(1, &transient), Some(RETRY_BACKOFF_BASE));
    assert_eq!(retry_backoff(2, &transient), Some(RETRY_BACKOFF_BASE * 2));
    assert_eq!(retry_backoff(MAX_BLOB_ATTEMPTS, &transient), None);
}

#[test]
fn retry_backoff_never_retries_permanent_errors() {
    // Even on the first attempt, a permanent error is not retried.
    assert_eq!(
        retry_backoff(1, &RegistryError::NotFound("ref".into())),
        None
    );
}

/// Round-trips for the OCI 1.1 referrers flows (OCI-4b) against the
/// in-process test registry — once with the referrers API served, once with
/// it disabled so the `<algo>-<hex>` fallback tag schema is exercised.
#[cfg(feature = "test-server")]
mod referrers_round_trips {
    use bytes::Bytes;
    use oci_client::client::{ClientConfig, ClientProtocol};
    use tempfile::tempdir;

    use super::*;
    use crate::image::{
        ArtifactBlob, ImageConfig, emit_artifact_manifest, emit_image, subject_from_entry,
    };
    use crate::registry::referrers::{ReferrersIndex, fallback_tag};
    use crate::test_registry::TestRegistry;

    /// Plain-HTTP client (the test server speaks HTTP on 127.0.0.1).
    fn http_client() -> RegistryClient {
        RegistryClient::with_config(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        })
    }

    struct Pushed {
        _dir: tempfile::TempDir,
        layout: ImageLayout,
        client: RegistryClient,
        reference: Reference,
        subject: ImageIndexEntry,
        artifact: ImageIndexEntry,
    }

    /// Emit a subject image plus one SBOM-shaped referrer, push both, and
    /// hand back everything a listing assertion needs.
    async fn push_subject_and_referrer(endpoint: &str) -> Pushed {
        let dir = tempdir().expect("layout tempdir");
        let layout = ImageLayout::init(dir.path()).expect("init layout");
        let ref_name = format!("{endpoint}/app:1");
        let subject =
            emit_image(&layout, &[], &ImageConfig::default(), &ref_name).expect("emit subject");
        let blob = ArtifactBlob {
            media_type: "application/spdx+json".to_string(),
            data: Bytes::from_static(b"{\"spdxVersion\":\"SPDX-2.3\"}"),
            annotations: None,
        };
        let artifact = emit_artifact_manifest(
            &layout,
            "application/spdx+json",
            Some(&subject_from_entry(&subject)),
            std::slice::from_ref(&blob),
            None,
            None,
        )
        .expect("emit artifact");

        let client = http_client();
        let reference: Reference = ref_name.parse().expect("parse reference");
        client
            .push(&reference, &ref_name, &layout, &RegistryAuth::Anonymous)
            .await
            .expect("push subject image");
        client
            .push_referrer(&reference, &artifact, &layout, &RegistryAuth::Anonymous)
            .await
            .expect("push referrer");

        Pushed {
            _dir: dir,
            layout,
            client,
            reference,
            subject,
            artifact,
        }
    }

    fn digest_ref(reference: &Reference, digest: &str) -> Reference {
        Reference::with_digest(
            reference.registry().to_string(),
            reference.repository().to_string(),
            digest.to_string(),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn referrer_lists_via_the_referrers_api() {
        let registry = TestRegistry::start().await.expect("start test registry");
        let p = push_subject_and_referrer(registry.endpoint()).await;
        let subject_ref = digest_ref(&p.reference, &p.subject.digest);

        let listed = p
            .client
            .list_referrers(&subject_ref, &RegistryAuth::Anonymous, None)
            .await
            .expect("list referrers");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].digest, p.artifact.digest);

        // Server-side filtering passes through; matches backfill the type.
        let filtered = p
            .client
            .list_referrers(
                &subject_ref,
                &RegistryAuth::Anonymous,
                Some("application/spdx+json"),
            )
            .await
            .expect("filtered listing");
        assert_eq!(filtered.len(), 1);
        assert_eq!(
            filtered[0].artifact_type.as_deref(),
            Some("application/spdx+json")
        );

        let none = p
            .client
            .list_referrers(
                &subject_ref,
                &RegistryAuth::Anonymous,
                Some("application/never"),
            )
            .await
            .expect("non-matching filter");
        assert!(none.is_empty());

        // Nothing refers to the artifact itself.
        let artifact_ref = digest_ref(&p.reference, &p.artifact.digest);
        assert!(
            p.client
                .list_referrers(&artifact_ref, &RegistryAuth::Anonymous, None)
                .await
                .expect("empty listing")
                .is_empty()
        );

        registry.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn referrer_falls_back_to_the_tag_schema() {
        let registry = TestRegistry::start_without_referrers_api()
            .await
            .expect("start test registry without referrers API");
        let p = push_subject_and_referrer(registry.endpoint()).await;
        let subject_ref = digest_ref(&p.reference, &p.subject.digest);

        // push_referrer maintained the fallback tag: it must hold a
        // parseable referrers index naming the artifact.
        let fallback_ref = Reference::with_tag(
            p.reference.registry().to_string(),
            p.reference.repository().to_string(),
            fallback_tag(&p.subject.digest).expect("fallback tag"),
        );
        let (bytes, _digest) = p
            .client
            .inner()
            .pull_manifest_raw(
                &fallback_ref,
                &RegistryAuth::Anonymous,
                &[OCI_IMAGE_INDEX_MEDIA_TYPE],
            )
            .await
            .expect("fallback tag was created");
        let on_wire: ReferrersIndex = serde_json::from_slice(&bytes).expect("parse fallback");
        assert_eq!(on_wire.manifests.len(), 1);
        assert_eq!(on_wire.manifests[0].digest, p.artifact.digest);

        // list_referrers reads the fallback transparently, type intact.
        let listed = p
            .client
            .list_referrers(&subject_ref, &RegistryAuth::Anonymous, None)
            .await
            .expect("list via fallback");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].digest, p.artifact.digest);
        assert_eq!(
            listed[0].artifact_type.as_deref(),
            Some("application/spdx+json"),
            "fallback descriptors carry artifactType",
        );

        // A second referrer read-modify-writes the tag: both listed, sorted.
        let signature = emit_artifact_manifest(
            &p.layout,
            "application/vnd.example.signature.v1",
            Some(&subject_from_entry(&p.subject)),
            &[],
            None,
            None,
        )
        .expect("emit signature artifact");
        p.client
            .push_referrer(
                &p.reference,
                &signature,
                &p.layout,
                &RegistryAuth::Anonymous,
            )
            .await
            .expect("push second referrer");

        let listed = p
            .client
            .list_referrers(&subject_ref, &RegistryAuth::Anonymous, None)
            .await
            .expect("list two referrers");
        assert_eq!(listed.len(), 2);
        let mut digests: Vec<&str> = listed.iter().map(|d| d.digest.as_str()).collect();
        let mut expected = [p.artifact.digest.as_str(), signature.digest.as_str()];
        digests.sort_unstable();
        expected.sort_unstable();
        assert_eq!(digests, expected);

        // Client-side filtering on the fallback path.
        let only_sig = p
            .client
            .list_referrers(
                &subject_ref,
                &RegistryAuth::Anonymous,
                Some("application/vnd.example.signature.v1"),
            )
            .await
            .expect("filtered fallback listing");
        assert_eq!(only_sig.len(), 1);
        assert_eq!(only_sig[0].digest, signature.digest);

        registry.shutdown().await;
    }
}
