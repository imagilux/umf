//! URL fetching for `ADD <url>` sources.
//!
//! Every `ADD https://… <dst>` in the recipe is fetched once, up front, in
//! the async phase of [`super::build`] — the directive walk itself is
//! synchronous and consumes the staged payloads from a map. Fetching every
//! build (rather than caching the download) is deliberate, mirroring
//! docker: the *layer* cache then keys on the payload's sha256, so an
//! unchanged remote still gets a layer-cache hit while a silently-changed
//! one busts it.
//!
//! Security posture mirrors the registry client's blob handling: the
//! response is streamed to a tempfile with a hard size ceiling (never
//! buffered unbounded in memory), the digest is computed during the
//! stream, redirects are bounded by reqwest's default policy, and TLS is
//! rustls — the same stack the registry client already trusts.

use std::io::Write as _;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use tracing::info;

use super::EngineBuildError;

/// Hard ceiling for a fetched `ADD <url>` payload — same value as the
/// registry client's per-blob cap, and for the same reason: a hostile or
/// misconfigured server must not be able to fill the disk.
pub(crate) const MAX_URL_FETCH_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// How long to wait for a TCP/TLS connection before giving up. Transfers
/// themselves are not time-limited — large payloads on slow links are
/// legitimate — but they are size-capped.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A fetched `ADD <url>` payload, staged on disk.
///
/// The tempfile guard keeps the payload alive for the duration of the
/// build; `digest` is the sha256 of the payload bytes, computed while
/// streaming, and feeds the layer-cache key.
#[derive(Debug)]
pub(crate) struct FetchedUrl {
    /// The payload bytes, staged in a tempfile.
    pub(crate) file: NamedTempFile,
    /// `sha256:<hex>` of the payload.
    pub(crate) digest: String,
}

/// Fetch `url` into a tempfile, computing its digest along the way.
pub(crate) async fn fetch_url(url: &str) -> Result<FetchedUrl, EngineBuildError> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| fetch_failed(url, &e.to_string()))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| fetch_failed(url, &e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(fetch_failed(url, &format!("HTTP {status}")));
    }

    let mut file = NamedTempFile::new()?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| fetch_failed(url, &e.to_string()))?
    {
        total += chunk.len() as u64;
        if total > MAX_URL_FETCH_BYTES {
            return Err(fetch_failed(
                url,
                &format!("payload exceeds the {MAX_URL_FETCH_BYTES}-byte ceiling"),
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.flush()?;

    let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
    info!(url = %url, bytes = total, digest = %digest, "ADD url: fetched");
    Ok(FetchedUrl { file, digest })
}

fn fetch_failed(url: &str, detail: &str) -> EngineBuildError {
    EngineBuildError::AddUrlFetchFailed {
        url: url.to_string(),
        detail: detail.to_string(),
    }
}

/// The filename a URL implies for file-at-destination placement: the last
/// path segment, query/fragment stripped — `https://h/p/app.tar.gz?x=1`
/// names `app.tar.gz`. Falls back to `"download"` for a bare authority.
pub(crate) fn url_leaf(url: &str) -> String {
    let no_fragment = url.split('#').next().unwrap_or(url);
    let no_query = no_fragment.split('?').next().unwrap_or(no_fragment);
    let after_scheme = no_query.split_once("://").map_or(no_query, |(_, r)| r);
    match after_scheme.rsplit('/').next() {
        Some(leaf) if !leaf.is_empty() && leaf != after_scheme => leaf.to_string(),
        _ => "download".to_string(),
    }
}

#[cfg(test)]
mod tests;
