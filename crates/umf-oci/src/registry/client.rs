//! Async registry client.
//!
//! [`RegistryClient`] wraps [`oci_client::Client`] with two materialisation
//! operations against an [`ImageLayout`]:
//!
//! * [`RegistryClient::pull`] — resolve a registry reference, write the
//!   manifest and every referenced blob into the on-disk layout, and record
//!   the `ref.name → manifest digest` association in `index.json`.
//! * [`RegistryClient::push`] — read a manifest from `index.json` by ref name,
//!   upload every referenced blob (config + layers, or each child manifest in
//!   the index case), then upload the manifest itself preserving the original
//!   byte representation.
//!
//! The byte-for-byte preservation matters: the manifest digest is content-
//! addressed over the raw bytes, so any re-serialisation would change the
//! digest and break round-trip equality.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::HeaderValue;
use oci_client::Reference;
use oci_client::client::ClientConfig;
use oci_client::manifest::{
    IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE, ImageIndexEntry,
    OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE, OciDescriptor, OciImageIndex,
    OciImageManifest, OciManifest,
};
use oci_client::secrets::RegistryAuth;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::registry::error::RegistryError;
use crate::registry::layout::{ImageLayout, REF_NAME_ANNOTATION, sha256_digest};
use crate::registry::referrers::{ReferrerDescriptor, ReferrersIndex, fallback_tag};

/// Media types accepted when pulling a manifest by reference.
///
/// Covers OCI v1 image + index manifests and the legacy distribution-v2 forms
/// (`application/vnd.docker.distribution.manifest.v2+json`,
/// `application/vnd.docker.distribution.manifest.list.v2+json`), so we can
/// transparently consume images published in either flavour.
pub const ACCEPTED_MANIFEST_MEDIA_TYPES: &[&str] = &[
    OCI_IMAGE_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE,
    IMAGE_MANIFEST_MEDIA_TYPE,
    IMAGE_MANIFEST_LIST_MEDIA_TYPE,
];

/// Hard per-blob ceiling for a single pulled blob. A hostile or MITM'd registry
/// can advertise a tiny `size` in the manifest and then stream unbounded bytes;
/// 8 GiB comfortably exceeds any real layer/config blob while bounding the
/// worst-case memory a single blob can consume.
const MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Byte ceiling to enforce while pulling a blob of declared `size`: the smaller
/// of the declared size and [`MAX_BLOB_BYTES`]. A non-positive declared size
/// (absent / bogus) falls back to the hard ceiling.
fn blob_byte_cap(declared_size: i64) -> u64 {
    let declared = u64::try_from(declared_size).unwrap_or(0);
    if declared == 0 {
        MAX_BLOB_BYTES
    } else {
        declared.min(MAX_BLOB_BYTES)
    }
}

/// A [`tokio::io::AsyncWrite`] that appends into an in-memory buffer but fails
/// the moment more than `cap` bytes are written — so an over-sized or
/// over-streaming blob is rejected *before* the rest is buffered, rather than
/// growing the buffer unboundedly to OOM.
struct CappedBuf {
    buf: Vec<u8>,
    cap: u64,
}

impl CappedBuf {
    fn with_cap(cap: u64) -> Self {
        // Pre-allocate modestly — never the (attacker-controlled) declared size.
        let initial = cap.min(8 * 1024 * 1024) as usize;
        Self {
            buf: Vec::with_capacity(initial),
            cap,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl tokio::io::AsyncWrite for CappedBuf {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.buf.len() as u64 + data.len() as u64 > self.cap {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("blob exceeds its {}-byte size ceiling", self.cap),
            )));
        }
        self.buf.extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// High-level OCI registry client.
///
/// Default idle read timeout for registry HTTP traffic. This is reqwest's
/// per-read-operation timeout, so a slow-but-progressing large-layer pull is
/// unaffected; only a wedged or half-open socket trips it. Overridable via
/// `UMF_REGISTRY_TIMEOUT` (whole seconds) for slow air-gapped mirrors.
const REGISTRY_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Default TCP connect timeout for registry HTTP traffic. Mirrors the
/// `ADD <url>` fetch path's connect cap so a black-holed registry host fails
/// fast instead of hanging the CLI forever.
const REGISTRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum attempts (initial try plus retries) for an idempotent blob
/// pull/push before giving up. Blobs are content-addressed (verified by
/// digest), so a retry only re-fetches or re-uploads the same bytes.
const MAX_BLOB_ATTEMPTS: u32 = 3;

/// Base backoff between blob retries, doubled each attempt (0.5s, 1s, ...).
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Build the default [`ClientConfig`] for UMF registry traffic: anonymous
/// auth, rustls-tls, and system trust roots (as `ClientConfig::default`), plus
/// the connect/read timeouts a bare default leaves unset (`None`). Without them
/// a stalled or black-holed registry hangs the CLI indefinitely. The read
/// timeout honours a `UMF_REGISTRY_TIMEOUT` override (whole seconds).
#[must_use]
pub fn default_client_config() -> ClientConfig {
    ClientConfig {
        read_timeout: Some(registry_read_timeout()),
        connect_timeout: Some(REGISTRY_CONNECT_TIMEOUT),
        ..ClientConfig::default()
    }
}

/// Read timeout for registry traffic, honouring a `UMF_REGISTRY_TIMEOUT`
/// override (whole seconds, must be greater than zero) and otherwise falling
/// back to [`REGISTRY_READ_TIMEOUT`].
fn registry_read_timeout() -> Duration {
    std::env::var("UMF_REGISTRY_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map_or(REGISTRY_READ_TIMEOUT, Duration::from_secs)
}

/// Decide whether a failed blob attempt should be retried: returns the backoff
/// to wait before the next attempt, or `None` to give up (attempts exhausted,
/// or the error is not transient). Shared by the pull and push retry loops.
fn retry_backoff(attempt: u32, err: &RegistryError) -> Option<Duration> {
    if attempt < MAX_BLOB_ATTEMPTS && err.is_transient() {
        Some(RETRY_BACKOFF_BASE * 2u32.pow(attempt - 1))
    } else {
        None
    }
}

/// Wraps [`oci_client::Client`] with UMF-shaped pull/push operations that
/// integrate with the on-disk [`ImageLayout`] cache.
#[derive(Clone)]
pub struct RegistryClient {
    inner: oci_client::Client,
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryClient").finish_non_exhaustive()
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryClient {
    /// Construct a default client.
    ///
    /// Uses anonymous auth, rustls-tls, system trust roots, and UMF's default
    /// connect/read timeouts (see [`default_client_config`]).
    pub fn new() -> Self {
        Self::with_config(default_client_config())
    }

    /// Construct a client with a custom [`ClientConfig`].
    ///
    /// Use this to plumb in custom CA roots, a non-default protocol (for plain-HTTP
    /// local registries), or to pin a specific HTTP behaviour.
    pub fn with_config(config: ClientConfig) -> Self {
        Self {
            inner: oci_client::Client::new(config),
        }
    }

    /// Access the underlying [`oci_client::Client`] for advanced use cases.
    pub fn inner(&self) -> &oci_client::Client {
        &self.inner
    }

    /// Pull `reference` and materialise it into `layout`.
    ///
    /// On success the layout contains every blob transitively reachable from the
    /// manifest, and `index.json` carries an entry annotated with the reference's
    /// stringified form pointing at the manifest digest.
    ///
    /// Both single-image and image-index manifests are supported. When the
    /// resolved manifest is an index, every child manifest and its transitively
    /// referenced blobs are pulled as well — the caller can select a platform
    /// later, against the cached layout, without further network access.
    // `debug`, not `info`: at the default (info) level the span's inline
    // context would duplicate the `reference` that the `info!("pulling")` event
    // below already reports. The span (and its grouping) surfaces at
    // `--trace-level debug` and in the json / pretty formats.
    #[tracing::instrument(
        level = "debug",
        name = "umf.oci.pull",
        skip(self, auth, layout),
        fields(reference = %reference)
    )]
    pub async fn pull(
        &self,
        reference: &Reference,
        auth: &RegistryAuth,
        layout: &ImageLayout,
    ) -> Result<ImageIndexEntry, RegistryError> {
        info!(reference = %reference, "pulling");
        let entry = self.pull_manifest_tree(reference, auth, layout).await?;
        let ref_name = reference.whole();
        layout.upsert_ref(&ref_name, entry.clone())?;
        Ok(entry)
    }

    /// Push the manifest for `ref_name` from `layout` to `reference`.
    ///
    /// Blobs are pushed before the manifest in accordance with the OCI
    /// distribution spec. For image-index manifests, each child manifest's
    /// blob tree is pushed first, then the child manifest itself, and finally
    /// the index. The manifest is uploaded as its original on-disk bytes, so
    /// its digest is preserved.
    // `debug` for the same reason as `pull` above: keep the default info
    // log clean of a span prefix that duplicates the event's own fields.
    #[tracing::instrument(
        level = "debug",
        name = "umf.oci.push",
        skip(self, layout, auth),
        fields(reference = %reference, ref_name = %ref_name)
    )]
    pub async fn push(
        &self,
        reference: &Reference,
        ref_name: &str,
        layout: &ImageLayout,
        auth: &RegistryAuth,
    ) -> Result<(), RegistryError> {
        let entry = layout
            .lookup_ref(ref_name)?
            .ok_or_else(|| RegistryError::NotFound(ref_name.to_string()))?;
        info!(reference = %reference, manifest = %entry.digest, "pushing");

        // Establish auth context for both push and pull (chunked push setup may
        // need a pull-scoped token first).
        self.inner
            .auth(reference, auth, oci_client::RegistryOperation::Push)
            .await?;

        self.push_manifest_tree(reference, layout, &entry).await
    }

    /// List the referrers of `subject` — a **digest-pinned** reference —
    /// trying the OCI 1.1 referrers API first and falling back to the
    /// `<algo>-<hex>` tag schema when the registry does not serve it
    /// (OCI-4b).
    ///
    /// With `artifact_type` set, the API path filters server-side and the
    /// fallback path client-side, per spec. Descriptors from the fallback
    /// path always carry the referrer's `artifactType`; an *unfiltered*
    /// listing served by the API path leaves it `None` — `oci-client` 0.16's
    /// typed index drops the per-descriptor field — while a filtered one
    /// backfills it from the filter (every match carries it by definition).
    #[tracing::instrument(
        level = "info",
        name = "umf.oci.list_referrers",
        skip(self, auth),
        fields(subject = %subject)
    )]
    pub async fn list_referrers(
        &self,
        subject: &Reference,
        auth: &RegistryAuth,
        artifact_type: Option<&str>,
    ) -> Result<Vec<ReferrerDescriptor>, RegistryError> {
        let digest = subject.digest().ok_or_else(|| {
            RegistryError::MalformedDigest(format!(
                "{subject}: a referrers listing needs a digest-pinned reference"
            ))
        })?;
        self.inner
            .auth(subject, auth, oci_client::RegistryOperation::Pull)
            .await?;
        match self.inner.pull_referrers(subject, artifact_type).await {
            Ok(index) => Ok(index
                .manifests
                .into_iter()
                .map(|e| ReferrerDescriptor {
                    media_type: e.media_type,
                    digest: e.digest,
                    size: e.size,
                    artifact_type: artifact_type.map(str::to_string),
                    annotations: e.annotations,
                })
                .collect()),
            Err(e) if is_not_found(&e) => {
                debug!("referrers API absent, reading the fallback tag");
                let index = self.pull_fallback_index(subject, auth, digest).await?;
                Ok(index.filtered(artifact_type))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Push the referrer `artifact` — an entry returned by
    /// [`crate::image::emit_artifact_manifest`], whose manifest must carry a
    /// `subject` — into `repository`'s repo, digest-addressed, and make it
    /// listable (OCI-4b).
    ///
    /// A registry serving the OCI 1.1 referrers API indexes subject-bearing
    /// manifests on push, so the push alone suffices there. When the API is
    /// absent the spec puts the burden on the client: the `<algo>-<hex>`
    /// fallback tag is read-modified-written here. That read-modify-write is
    /// last-writer-wins under concurrent pushers — a spec-acknowledged
    /// limitation of the fallback schema.
    ///
    /// The subject itself is not pushed; pushing the referrer before (or
    /// without) its subject is legal per spec.
    #[tracing::instrument(
        level = "info",
        name = "umf.oci.push_referrer",
        skip(self, layout, auth),
        fields(repository = %repository, artifact = %artifact.digest)
    )]
    pub async fn push_referrer(
        &self,
        repository: &Reference,
        artifact: &ImageIndexEntry,
        layout: &ImageLayout,
        auth: &RegistryAuth,
    ) -> Result<(), RegistryError> {
        let manifest_bytes = layout.read_blob(&artifact.digest)?;
        let manifest: OciImageManifest = serde_json::from_slice(&manifest_bytes)?;
        let Some(subject) = manifest.subject.as_ref() else {
            return Err(RegistryError::InvalidLayout(format!(
                "manifest {} carries no subject — not a referrer",
                artifact.digest
            )));
        };

        let artifact_ref = Reference::with_digest(
            repository.registry().to_string(),
            repository.repository().to_string(),
            artifact.digest.clone(),
        );
        self.inner
            .auth(&artifact_ref, auth, oci_client::RegistryOperation::Push)
            .await?;
        self.push_manifest_tree(&artifact_ref, layout, artifact)
            .await?;

        // Probe for the referrers API; absent → maintain the fallback tag.
        let subject_ref = Reference::with_digest(
            repository.registry().to_string(),
            repository.repository().to_string(),
            subject.digest.clone(),
        );
        match self.inner.pull_referrers(&subject_ref, None).await {
            // API present: the registry indexed the subject-bearing push.
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => {
                debug!("referrers API absent, maintaining the fallback tag");
                let mut index = self
                    .pull_fallback_index(&subject_ref, auth, &subject.digest)
                    .await?;
                index.upsert(ReferrerDescriptor {
                    media_type: artifact.media_type.clone(),
                    digest: artifact.digest.clone(),
                    size: artifact.size,
                    artifact_type: manifest.artifact_type.clone(),
                    annotations: manifest.annotations.clone(),
                });
                let fallback_ref = Reference::with_tag(
                    repository.registry().to_string(),
                    repository.repository().to_string(),
                    fallback_tag(&subject.digest)?,
                );
                let body = serde_json::to_vec(&index)?;
                self.inner
                    .push_manifest_raw(
                        &fallback_ref,
                        body,
                        HeaderValue::from_static(OCI_IMAGE_INDEX_MEDIA_TYPE),
                    )
                    .await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Read the fallback referrers tag for `digest`, treating a missing tag
    /// as the empty index — the state every fallback maintenance starts from.
    async fn pull_fallback_index(
        &self,
        subject: &Reference,
        auth: &RegistryAuth,
        digest: &str,
    ) -> Result<ReferrersIndex, RegistryError> {
        let fallback_ref = Reference::with_tag(
            subject.registry().to_string(),
            subject.repository().to_string(),
            fallback_tag(digest)?,
        );
        match self
            .inner
            .pull_manifest_raw(&fallback_ref, auth, &[OCI_IMAGE_INDEX_MEDIA_TYPE])
            .await
        {
            Ok((bytes, _digest)) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if is_not_found(&e) => Ok(ReferrersIndex::empty()),
            Err(e) => Err(e.into()),
        }
    }

    fn pull_manifest_tree<'a>(
        &'a self,
        reference: &'a Reference,
        auth: &'a RegistryAuth,
        layout: &'a ImageLayout,
    ) -> Pin<Box<dyn Future<Output = Result<ImageIndexEntry, RegistryError>> + Send + 'a>> {
        Box::pin(async move {
            let (manifest_bytes, manifest_digest) = self
                .inner
                .pull_manifest_raw(reference, auth, ACCEPTED_MANIFEST_MEDIA_TYPES)
                .await?;

            // Integrity model: `oci-client` verifies `manifest_bytes` against
            // the digest pinned in a `…@sha256:` reference or the registry's
            // `Docker-Content-Digest` response header. Pulling by a mutable
            // *tag* from a registry that sends neither leaves the manifest
            // itself unanchored — this recompute is then only a self-check.
            // Every blob the manifest references is still digest-verified
            // (`pull_blob` + `write_blob_with_digest`), so the worst a hostile
            // or MITM'd registry can do over an untrusted transport is serve a
            // *different but internally-consistent* image for the tag. Defence:
            // TLS (the default) prevents MITM, compliant registries send the
            // header, and integrity-critical callers should pin by `@sha256:`.
            let computed = sha256_digest(&manifest_bytes);
            if computed != manifest_digest {
                return Err(RegistryError::DigestMismatch {
                    expected: manifest_digest,
                    found: computed,
                });
            }

            let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)?;
            layout.write_blob_with_digest(&manifest_bytes, &manifest_digest)?;

            let manifest_media_type = match &manifest {
                OciManifest::Image(m) => m
                    .media_type
                    .clone()
                    .unwrap_or_else(|| OCI_IMAGE_MEDIA_TYPE.to_string()),
                OciManifest::ImageIndex(i) => i
                    .media_type
                    .clone()
                    .unwrap_or_else(|| OCI_IMAGE_INDEX_MEDIA_TYPE.to_string()),
            };

            match &manifest {
                OciManifest::Image(image) => {
                    self.pull_image_blobs(reference, auth, image, layout)
                        .await?;
                }
                OciManifest::ImageIndex(index) => {
                    self.pull_index_children(reference, auth, index, layout)
                        .await?;
                }
            }

            Ok(ImageIndexEntry {
                media_type: manifest_media_type,
                digest: manifest_digest,
                size: manifest_bytes.len() as i64,
                platform: None,
                annotations: None,
            })
        })
    }

    async fn pull_image_blobs(
        &self,
        reference: &Reference,
        auth: &RegistryAuth,
        manifest: &OciImageManifest,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        self.pull_blob_to_layout(reference, auth, &manifest.config, layout)
            .await?;
        for layer in &manifest.layers {
            self.pull_blob_to_layout(reference, auth, layer, layout)
                .await?;
        }
        Ok(())
    }

    async fn pull_index_children(
        &self,
        reference: &Reference,
        auth: &RegistryAuth,
        index: &OciImageIndex,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        for entry in &index.manifests {
            let child = reference.clone_with_digest(entry.digest.clone());
            // Recurse but do not upsert a ref — children are referenced only
            // by their digest in the parent index.
            self.pull_manifest_tree(&child, auth, layout).await?;
        }
        Ok(())
    }

    async fn pull_blob_to_layout(
        &self,
        reference: &Reference,
        auth: &RegistryAuth,
        descriptor: &OciDescriptor,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        if layout.has_blob(&descriptor.digest) {
            debug!(digest = %descriptor.digest, "blob already cached");
            return Ok(());
        }
        // Bounded retry on transient transport errors: a single connection
        // reset or 5xx mid-pull shouldn't abort an entire (multi-GB) build. The
        // pull is idempotent (the blob is content-addressed and re-verified by
        // digest), so re-fetching the same bytes is safe.
        let mut attempt = 1;
        loop {
            match self
                .pull_blob_attempt(reference, auth, descriptor, layout)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => match retry_backoff(attempt, &e) {
                    Some(backoff) => {
                        warn!(
                            error = %e,
                            attempt,
                            digest = %descriptor.digest,
                            backoff_ms = backoff.as_millis() as u64,
                            "transient registry error pulling blob; retrying",
                        );
                        sleep(backoff).await;
                        attempt += 1;
                    }
                    None => return Err(e),
                },
            }
        }
    }

    /// One attempt at pulling a single blob into the layout (auth + capped
    /// stream + digest-verified write). Retried by [`Self::pull_blob_to_layout`].
    async fn pull_blob_attempt(
        &self,
        reference: &Reference,
        auth: &RegistryAuth,
        descriptor: &OciDescriptor,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        // Ensure auth context is established before pulling the blob.
        self.inner
            .auth(reference, auth, oci_client::RegistryOperation::Pull)
            .await?;

        // Stream the blob through a capped sink: a hostile or MITM'd registry
        // can advertise a tiny `size` and then stream unbounded bytes, which the
        // previous `Vec::with_capacity(size)` + `pull_blob` buffered straight to
        // OOM (the declared size was never enforced as a ceiling on bytes
        // actually read). Cap at the smaller of the declared size and
        // `MAX_BLOB_BYTES` and abort the instant the stream exceeds it.
        // `write_blob_with_digest` still verifies the content against its digest
        // on the way into the cache, so integrity is unchanged.
        let mut sink = CappedBuf::with_cap(blob_byte_cap(descriptor.size));
        self.inner
            .pull_blob(reference, descriptor, &mut sink)
            .await?;
        layout.write_blob_with_digest(&sink.into_inner(), &descriptor.digest)
    }

    fn push_manifest_tree<'a>(
        &'a self,
        reference: &'a Reference,
        layout: &'a ImageLayout,
        entry: &'a ImageIndexEntry,
    ) -> Pin<Box<dyn Future<Output = Result<(), RegistryError>> + Send + 'a>> {
        Box::pin(async move {
            let manifest_bytes = layout.read_blob(&entry.digest)?;
            let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)?;

            match &manifest {
                OciManifest::Image(image) => {
                    self.push_image_blobs(reference, image, layout).await?;
                }
                OciManifest::ImageIndex(index) => {
                    for child in &index.manifests {
                        self.push_manifest_tree(reference, layout, child).await?;
                    }
                }
            }

            let content_type = HeaderValue::from_str(&entry.media_type).map_err(|e| {
                RegistryError::InvalidLayout(format!(
                    "invalid manifest mediaType {:?}: {e}",
                    entry.media_type
                ))
            })?;
            let pushed = self
                .inner
                .push_manifest_raw(reference, manifest_bytes, content_type)
                .await?;
            debug!(url = %pushed, "manifest pushed");
            Ok(())
        })
    }

    async fn push_image_blobs(
        &self,
        reference: &Reference,
        manifest: &OciImageManifest,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        self.push_blob_from_layout(reference, &manifest.config, layout)
            .await?;
        for layer in &manifest.layers {
            self.push_blob_from_layout(reference, layer, layout).await?;
        }
        Ok(())
    }

    async fn push_blob_from_layout(
        &self,
        reference: &Reference,
        descriptor: &OciDescriptor,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        // Bounded retry on transient transport errors (symmetric with the pull
        // path): a single 5xx or reset shouldn't abort a multi-GB push. The push
        // is idempotent (the blob is content-addressed and the registry dedups /
        // re-verifies by digest), so re-uploading the same bytes is safe, and
        // `blob_chunk_stream` re-opens the file for each attempt.
        let mut attempt = 1;
        loop {
            match self.push_blob_attempt(reference, descriptor, layout).await {
                Ok(()) => return Ok(()),
                Err(e) => match retry_backoff(attempt, &e) {
                    Some(backoff) => {
                        warn!(
                            error = %e,
                            attempt,
                            digest = %descriptor.digest,
                            backoff_ms = backoff.as_millis() as u64,
                            "transient registry error pushing blob; retrying",
                        );
                        sleep(backoff).await;
                        attempt += 1;
                    }
                    None => return Err(e),
                },
            }
        }
    }

    /// One attempt at streaming a single blob from the layout to the registry.
    /// Retried by [`Self::push_blob_from_layout`].
    async fn push_blob_attempt(
        &self,
        reference: &Reference,
        descriptor: &OciDescriptor,
        layout: &ImageLayout,
    ) -> Result<(), RegistryError> {
        // Stream the blob from its on-disk path in fixed-size chunks instead of
        // buffering the whole (possibly multi-GB) layer into a `Vec` via the
        // re-hashing `read_blob`. The blob was already content-verified when it
        // was written into the layout (`write_blob_with_digest`), so re-reading
        // and re-hashing it here was redundant; `push_blob_stream` uploads the
        // chunks as they are read, and the registry verifies the final digest
        // against `descriptor.digest` server-side. Peak RSS for a push is now
        // bounded by the chunk size, not the largest layer.
        let path = layout.blob_path(&descriptor.digest)?;
        let stream = blob_chunk_stream(path);
        let pushed = self
            .inner
            .push_blob_stream(reference, stream, &descriptor.digest)
            .await?;
        debug!(url = %pushed, digest = %descriptor.digest, "blob pushed");
        Ok(())
    }
}

/// Size of each chunk read from an on-disk blob while streaming it to a push.
/// Bounds the per-push working-set: only one such chunk is resident at a time,
/// independent of the layer's total size.
const PUSH_READ_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Build a [`Stream`] that reads the blob file at `path` in
/// [`PUSH_READ_CHUNK_BYTES`] chunks, yielding each as a [`bytes::Bytes`] for
/// [`oci_client::Client::push_blob_stream`].
///
/// IO failures (open or read) surface as
/// [`oci_client::errors::OciDistributionError::IoError`] so the push aborts
/// with the underlying error rather than silently truncating. The whole file
/// is never resident at once — at most one chunk plus the OS read-ahead.
fn blob_chunk_stream(
    path: std::path::PathBuf,
) -> impl futures::Stream<Item = Result<bytes::Bytes, oci_client::errors::OciDistributionError>> {
    // `state` carries the lazily-opened file across poll iterations; `None`
    // before the first poll, `Some(Err)` once a fatal IO error has been
    // emitted (after which the stream terminates).
    futures::stream::unfold(BlobStreamState::Unopened(path), |state| async move {
        match state {
            BlobStreamState::Done => None,
            BlobStreamState::Unopened(path) => match std::fs::File::open(&path) {
                Ok(file) => read_next_chunk(file),
                Err(e) => Some((Err(e.into()), BlobStreamState::Done)),
            },
            BlobStreamState::Open(file) => read_next_chunk(file),
        }
    })
}

enum BlobStreamState {
    Unopened(std::path::PathBuf),
    Open(std::fs::File),
    Done,
}

/// Read one chunk from `file`, returning the stream item + next state. `Ok` with
/// a non-empty chunk continues; a short/empty read ends the stream cleanly; an
/// IO error is emitted once and then terminates.
fn read_next_chunk(
    mut file: std::fs::File,
) -> Option<(
    Result<bytes::Bytes, oci_client::errors::OciDistributionError>,
    BlobStreamState,
)> {
    use std::io::Read as _;
    let mut buf = vec![0u8; PUSH_READ_CHUNK_BYTES];
    let mut filled = 0;
    // `read` may return short reads; loop until the chunk buffer is full or EOF
    // so we emit maximally-sized chunks (fewer push round-trips) deterministically.
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Some((Err(e.into()), BlobStreamState::Done)),
        }
    }
    if filled == 0 {
        return None; // clean EOF
    }
    buf.truncate(filled);
    Some((Ok(bytes::Bytes::from(buf)), BlobStreamState::Open(file)))
}

/// Construct an [`ImageIndexEntry`] tagged with the standard ref-name annotation.
///
/// Useful when callers want to manually populate an [`ImageLayout`]'s `index.json`
/// — for instance when staging a synthetic image for a test or for an
/// out-of-band import.
pub fn ref_annotated_entry(ref_name: &str, mut entry: ImageIndexEntry) -> ImageIndexEntry {
    let annotations = entry.annotations.get_or_insert_with(Default::default);
    annotations.insert(REF_NAME_ANNOTATION.to_string(), ref_name.to_string());
    entry
}

/// `true` for the transport errors a registry answers with when an endpoint
/// or manifest is absent — the referrers-API probe and the fallback-tag read
/// both branch to "not supported / not yet created" on these.
fn is_not_found(err: &oci_client::errors::OciDistributionError) -> bool {
    use oci_client::errors::OciDistributionError as E;
    matches!(
        err,
        E::ServerError { code: 404, .. } | E::ImageManifestNotFoundError(_)
    )
}

#[cfg(test)]
mod tests;
