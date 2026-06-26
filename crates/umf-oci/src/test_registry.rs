//! In-process OCI distribution v2 server for test fixtures.
//!
//! Replaces the `registry:2` container the round-trip tests used to
//! boot via podman with an HTTP server that runs inside the test
//! binary. Storage is in-memory — blobs and tag maps live in
//! `BTreeMap`s — so the server is fast, deterministic, and leaves no
//! state behind when the test finishes.
//!
//! ## Scope
//!
//! Implements the minimal subset of [OCI distribution spec v2][spec]
//! the workspace's pull + push path exercises:
//!
//! - `GET /v2/` → 200 (root readiness check).
//! - `HEAD/GET /v2/<name>/manifests/<ref>` → manifest fetch + headers.
//! - `PUT /v2/<name>/manifests/<ref>` → publish manifest.
//! - `HEAD/GET /v2/<name>/blobs/<digest>` → blob fetch.
//! - `GET /v2/<name>/referrers/<digest>` → OCI 1.1 referrers listing
//!   (disable with [`TestRegistry::start_without_referrers_api`] to test
//!   the fallback tag schema).
//! - `POST /v2/<name>/blobs/uploads/` → start upload (returns
//!   `Location: /v2/<name>/blobs/uploads/<uuid>`).
//! - `PATCH /v2/<name>/blobs/uploads/<uuid>` → append chunk.
//! - `PUT /v2/<name>/blobs/uploads/<uuid>?digest=<sha256:hex>` →
//!   finalise upload, validate the digest.
//!
//! Not implemented (deliberately — not exercised by our tests):
//!
//! - Auth (`/v2/` is unconditionally open).
//! - Cross-repo blob mounts.
//! - Catalog / tag-list endpoints.
//! - Range-resume uploads.
//!
//! ## Usage
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use umf_oci::test_registry::TestRegistry;
//!
//! let registry = TestRegistry::start().await?;
//! let endpoint = registry.endpoint();  // e.g. "127.0.0.1:32812"
//! // ... push/pull against `http://{endpoint}/v2/...` ...
//! registry.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! [spec]: https://github.com/opencontainers/distribution-spec/blob/main/spec.md

// `Response::builder().body(...)` returns Result but the error path is
// unreachable for the inputs the response handlers construct (status
// codes + well-formed header values from string literals). The lint
// policy treats `.expect` as a warn-to-deny in production code, but
// this module is test infrastructure (gated on the `test-server`
// feature, used only by round-trip integration tests) and the
// expect-on-infallible-builder shape is the idiomatic axum response.
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::{any, get};
use bytes::Bytes;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tracing::debug;

/// Handle to a running in-process OCI distribution v2 server.
///
/// Dropping the handle without calling [`Self::shutdown`] keeps the
/// server alive until the tokio runtime that owns the task is dropped
/// — fine for `#[tokio::test]`s where the runtime is per-test.
pub struct TestRegistry {
    endpoint: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl TestRegistry {
    /// Bind to a random localhost port and spawn the server in the
    /// background. The future resolves once the listener is ready to
    /// accept connections.
    ///
    /// # Errors
    /// Filesystem / socket errors during listener bind.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_inner(true).await
    }

    /// Like [`Self::start`], but with the OCI 1.1 referrers API disabled:
    /// `GET /v2/<name>/referrers/<digest>` answers 404 like a registry that
    /// predates the endpoint, so clients exercise the `<algo>-<hex>`
    /// fallback tag schema instead.
    pub async fn start_without_referrers_api() -> std::io::Result<Self> {
        Self::start_inner(false).await
    }

    async fn start_inner(referrers_api: bool) -> std::io::Result<Self> {
        let state = Arc::new(Mutex::new(RegistryState {
            referrers_api,
            ..RegistryState::default()
        }));
        let app = router(state);

        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let local_addr = listener.local_addr()?;
        let endpoint = format!("{}:{}", local_addr.ip(), local_addr.port());

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        debug!(%endpoint, "in-process OCI test registry listening");
        Ok(Self {
            endpoint,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        })
    }

    /// Endpoint string in `host:port` form. Construct URLs as
    /// `http://{endpoint}/v2/...`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Send the shutdown signal and wait for the server task to exit.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for TestRegistry {
    fn drop(&mut self) {
        // Best-effort shutdown on drop — the join handle outlives this
        // when the caller didn't explicitly await `shutdown()`; tokio
        // cleans up when the runtime ends.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

// ── State + routing ─────────────────────────────────────────────────────────

/// In-memory state. Blobs are keyed by their full
/// `sha256:<hex>` digest; tags map `(repo, reference)` → manifest
/// digest (the manifest itself is stored as a blob under that
/// digest). Uploads are keyed by the UUID we hand back via
/// `Location`.
#[derive(Debug, Default)]
struct RegistryState {
    /// Serve `GET /v2/<name>/referrers/<digest>` (the OCI 1.1 referrers
    /// API). When `false` the route answers 404 like a registry that
    /// predates the endpoint, forcing clients onto the fallback tag schema.
    referrers_api: bool,
    blobs: BTreeMap<String, Bytes>,
    /// `(repo, reference)` → manifest digest. Reference is either a
    /// tag (`latest`) or a digest (`sha256:...`).
    tags: BTreeMap<(String, String), String>,
    /// Pending uploads. UUID → accumulated bytes.
    uploads: BTreeMap<String, Vec<u8>>,
}

type Shared = Arc<Mutex<RegistryState>>;

fn router(state: Shared) -> Router {
    Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2", get(v2_root))
        // Catch-all on `/v2/*` so we can use a single dispatch function
        // that parses the trailing path. The OCI distribution endpoints
        // overlap in shape (`manifests/<ref>`, `blobs/<digest>`,
        // `blobs/uploads/<uuid>`) and axum's path-parameter matching
        // is fine for that.
        .route("/v2/{*tail}", any(dispatch))
        .with_state(state)
}

async fn v2_root() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Docker-Distribution-API-Version", "registry/2.0")
        .body(Body::empty())
        .expect("response builder")
}

#[derive(Debug, Deserialize)]
struct PutBlobQuery {
    digest: Option<String>,
}

async fn dispatch(
    State(state): State<Shared>,
    method: Method,
    AxumPath(tail): AxumPath<String>,
    Query(query): Query<PutBlobQuery>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    debug!(?method, %tail, body_len = body.len(), "test_registry: dispatch");
    let segments: Vec<&str> = tail.split('/').collect();
    // Repository name in distribution-spec is the path components up
    // to a known terminator (`manifests`, `blobs`). Find the
    // terminator index; everything before is the repo.
    let term_idx = segments
        .iter()
        .position(|s| *s == "manifests" || *s == "blobs" || *s == "referrers");
    let Some(term_idx) = term_idx else {
        debug!("test_registry: no terminator, 404");
        return not_found();
    };
    let repo = segments[..term_idx].join("/");
    let rest = &segments[term_idx..];
    debug!(repo = %repo, ?rest, "test_registry: matched");

    match (method, rest) {
        // ── Manifests ───────────────────────────────────────────────
        (Method::HEAD, ["manifests", reference]) => head_manifest(state, &repo, reference).await,
        (Method::GET, ["manifests", reference]) => get_manifest(state, &repo, reference).await,
        (Method::PUT, ["manifests", reference]) => {
            put_manifest(state, &repo, reference, &headers, body).await
        }

        // ── Referrers (OCI 1.1) ──────────────────────────────────────
        (Method::GET, ["referrers", digest]) => {
            let artifact_type = query_param(raw_query.as_deref(), "artifactType");
            get_referrers(state, &repo, digest, artifact_type.as_deref()).await
        }

        // ── Blobs ────────────────────────────────────────────────────
        (Method::HEAD, ["blobs", digest]) => head_blob(state, digest).await,
        (Method::GET, ["blobs", digest]) => get_blob(state, digest).await,

        // ── Uploads ──────────────────────────────────────────────────
        (Method::POST, ["blobs", "uploads"]) | (Method::POST, ["blobs", "uploads", ""]) => {
            start_upload(state, &repo).await
        }
        (Method::PATCH, ["blobs", "uploads", uuid]) => append_chunk(state, &repo, uuid, body).await,
        (Method::PUT, ["blobs", "uploads", uuid]) => {
            finalise_upload(state, &repo, uuid, query.digest.as_deref(), body).await
        }

        _ => not_found(),
    }
}

// ── Manifest endpoints ──────────────────────────────────────────────────────

async fn head_manifest(state: Shared, repo: &str, reference: &str) -> Response {
    let s = state.lock().await;
    let Some(digest) = s
        .tags
        .get(&(repo.to_string(), reference.to_string()))
        .cloned()
        .or_else(|| {
            reference
                .starts_with("sha256:")
                .then(|| reference.to_string())
        })
    else {
        return not_found();
    };
    let Some(bytes) = s.blobs.get(&digest).cloned() else {
        return not_found();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, manifest_content_type(&bytes))
        .header(header::CONTENT_LENGTH, bytes.len())
        .header("Docker-Content-Digest", digest)
        .body(Body::empty())
        .expect("response builder")
}

async fn get_manifest(state: Shared, repo: &str, reference: &str) -> Response {
    let s = state.lock().await;
    let Some(digest) = s
        .tags
        .get(&(repo.to_string(), reference.to_string()))
        .cloned()
        .or_else(|| {
            reference
                .starts_with("sha256:")
                .then(|| reference.to_string())
        })
    else {
        return not_found();
    };
    let Some(bytes) = s.blobs.get(&digest).cloned() else {
        return not_found();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, manifest_content_type(&bytes))
        .header(header::CONTENT_LENGTH, bytes.len())
        .header("Docker-Content-Digest", digest)
        .body(Body::from(bytes))
        .expect("response builder")
}

async fn put_manifest(
    state: Shared,
    repo: &str,
    reference: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let digest = compute_digest(&body);
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    let mut s = state.lock().await;
    s.blobs.insert(digest.clone(), body);
    s.tags
        .insert((repo.to_string(), reference.to_string()), digest.clone());
    drop(s);
    let location = format!("/v2/{repo}/manifests/{reference}");
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, location)
        .header(header::CONTENT_TYPE, content_type)
        .header("Docker-Content-Digest", digest)
        .body(Body::empty())
        .expect("response builder")
}

// ── Referrers endpoint (OCI 1.1) ────────────────────────────────────────────

/// Extract `key` from a raw query string with RFC 3986 semantics:
/// percent-decode only — a literal `+` stays a plus. Form-urlencoded
/// parsing (axum's `Query`) would turn `+` into a space, corrupting media
/// types like `application/spdx+json`, which `oci-client` 0.16 sends
/// unescaped in `?artifactType=`.
fn query_param(raw: Option<&str>, key: &str) -> Option<String> {
    raw?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Minimal `%XX` decoder (RFC 3986); malformed escapes pass through verbatim.
fn percent_decode(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| {
                let hi = char::from(bytes[i + 1]).to_digit(16)?;
                let lo = char::from(bytes[i + 2]).to_digit(16)?;
                Some((hi << 4 | lo) as u8)
            })
            .flatten();
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /v2/<name>/referrers/<digest>` — scan every manifest known to the
/// repo for a `subject` pointing at `subject_digest` and answer with an
/// image index whose descriptors carry each referrer's `artifactType` and
/// annotations, per the distribution spec. Filtering by `?artifactType=` is
/// honoured and advertised via `OCI-Filters-Applied`.
async fn get_referrers(
    state: Shared,
    repo: &str,
    subject_digest: &str,
    artifact_type: Option<&str>,
) -> Response {
    let s = state.lock().await;
    if !s.referrers_api {
        return not_found();
    }
    // Every manifest digest recorded for this repo (tag- and digest-pushed),
    // deduped; BTreeSet gives the response a deterministic order.
    let digests: BTreeSet<String> = s
        .tags
        .iter()
        .filter(|((r, _), _)| r == repo)
        .map(|(_, digest)| digest.clone())
        .collect();

    let mut manifests = Vec::new();
    for digest in digests {
        let Some(bytes) = s.blobs.get(&digest) else {
            continue;
        };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            continue;
        };
        if doc.pointer("/subject/digest").and_then(|v| v.as_str()) != Some(subject_digest) {
            continue;
        }
        let manifest_artifact_type = doc.get("artifactType").and_then(|v| v.as_str());
        if let Some(filter) = artifact_type
            && manifest_artifact_type != Some(filter)
        {
            continue;
        }
        let mut descriptor = serde_json::json!({
            "mediaType": doc
                .get("mediaType")
                .and_then(|v| v.as_str())
                .unwrap_or("application/vnd.oci.image.manifest.v1+json"),
            "digest": digest,
            "size": bytes.len(),
        });
        if let Some(at) = manifest_artifact_type {
            descriptor["artifactType"] = at.into();
        }
        if let Some(annotations) = doc.get("annotations") {
            descriptor["annotations"] = annotations.clone();
        }
        manifests.push(descriptor);
    }
    drop(s);

    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    });
    let bytes = serde_json::to_vec(&body).expect("serialize referrers index");
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/vnd.oci.image.index.v1+json",
        )
        .header(header::CONTENT_LENGTH, bytes.len());
    if artifact_type.is_some() {
        response = response.header("OCI-Filters-Applied", "artifactType");
    }
    response.body(Body::from(bytes)).expect("response builder")
}

fn manifest_content_type(bytes: &[u8]) -> &'static str {
    // Sniff the JSON for the `mediaType` field. Falls back to the OCI
    // image-manifest media type when nothing is recognised — the
    // pull client doesn't strictly require it but it's the polite
    // default.
    if let Ok(s) = std::str::from_utf8(bytes)
        && s.contains("application/vnd.oci.image.index.v1+json")
    {
        return "application/vnd.oci.image.index.v1+json";
    }
    "application/vnd.oci.image.manifest.v1+json"
}

// ── Blob endpoints ──────────────────────────────────────────────────────────

async fn head_blob(state: Shared, digest: &str) -> Response {
    let s = state.lock().await;
    let Some(bytes) = s.blobs.get(digest) else {
        return not_found();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header("Docker-Content-Digest", digest)
        .body(Body::empty())
        .expect("response builder")
}

async fn get_blob(state: Shared, digest: &str) -> Response {
    let s = state.lock().await;
    let Some(bytes) = s.blobs.get(digest).cloned() else {
        return not_found();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header("Docker-Content-Digest", digest)
        .body(Body::from(bytes))
        .expect("response builder")
}

// ── Upload endpoints ────────────────────────────────────────────────────────

async fn start_upload(state: Shared, repo: &str) -> Response {
    let uuid = uuid::Uuid::new_v4().to_string();
    {
        let mut s = state.lock().await;
        s.uploads.insert(uuid.clone(), Vec::new());
    }
    let location = format!("/v2/{repo}/blobs/uploads/{uuid}");
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(header::LOCATION, location)
        .header(
            "Docker-Upload-UUID",
            HeaderValue::from_str(&uuid).expect("uuid"),
        )
        .header("Range", "0-0")
        .body(Body::empty())
        .expect("response builder")
}

async fn append_chunk(state: Shared, repo: &str, uuid: &str, body: Bytes) -> Response {
    let mut s = state.lock().await;
    let Some(buf) = s.uploads.get_mut(uuid) else {
        return not_found();
    };
    buf.extend_from_slice(&body);
    let range = format!("0-{}", buf.len().saturating_sub(1));
    // The distribution spec requires PATCH to return a Location header
    // pointing at the upload session URL so the client knows where to
    // send the next chunk (or the final PUT).
    let location = format!("/v2/{repo}/blobs/uploads/{uuid}");
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(header::LOCATION, location)
        .header("Range", range)
        .header("Docker-Upload-UUID", uuid)
        .body(Body::empty())
        .expect("response builder")
}

async fn finalise_upload(
    state: Shared,
    repo: &str,
    uuid: &str,
    digest_query: Option<&str>,
    body: Bytes,
) -> Response {
    let Some(expected_digest) = digest_query else {
        return bad_request("missing digest query param");
    };
    let mut s = state.lock().await;
    let mut buf = match s.uploads.remove(uuid) {
        Some(buf) => buf,
        None => return not_found(),
    };
    if !body.is_empty() {
        buf.extend_from_slice(&body);
    }
    let actual = compute_digest(&buf);
    if actual != expected_digest {
        return bad_request(&format!(
            "digest mismatch: expected {expected_digest}, actual {actual}"
        ));
    }
    s.blobs.insert(actual.clone(), Bytes::from(buf));
    drop(s);
    // Distribution spec: blob upload finalisation must return Location
    // pointing at the blob's pull URL (`/v2/<repo>/blobs/<digest>`).
    let blob_url = format!("/v2/{repo}/blobs/{actual}");
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, blob_url)
        .header("Docker-Content-Digest", actual)
        .body(Body::empty())
        .expect("response builder")
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn compute_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("response builder")
}

fn bad_request(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from(msg.to_string()))
        .expect("response builder")
}

#[cfg(test)]
mod tests;
