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
//! stream, and TLS is rustls — the same stack the registry client already
//! trusts.
//!
//! Egress is governed by the same default-deny [`EgressPolicy`] that guards
//! `RUN`-step traffic, so a recipe cannot reach host-internal addresses
//! (loopback, link-local incl. the cloud-metadata IP, RFC1918, ULA, CGNAT)
//! through `ADD` when it cannot reach them through `RUN`. Two details make
//! that guarantee hold rather than merely look like it holds:
//!
//! - **The destination is resolved here and pinned.** The policy is checked
//!   against resolved addresses, then the chosen address is pinned into the
//!   client with `resolve()`, so the connection goes to the address that was
//!   vetted. Checking a name and letting the client re-resolve would leave a
//!   DNS-rebinding window between the check and the connect.
//! - **Redirects are followed manually.** reqwest's automatic policy would
//!   chase a `Location:` into a denied address without re-consulting us, so
//!   automatic redirects are disabled and each hop repeats the full
//!   resolve-check-pin cycle.
//!
//! The policy consulted is [`umf_engine::rootless::egress_policy`] — the one
//! the `RUN` sandbox already uses — so the operator escape hatch is the
//! existing `--rootless-net-allow` flag / `UMF_ROOTLESS_NET_ALLOW` variable,
//! not a second knob that could drift out of step with it.

use std::io::Write as _;
use std::net::IpAddr;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use tracing::{info, warn};
use umf_networking::ssrf::EgressPolicy;

use super::EngineBuildError;

/// How many `Location:` hops to follow before giving up. Matches reqwest's
/// own default so behaviour is unsurprising; the difference is that each hop
/// here is policy-checked.
const MAX_REDIRECTS: usize = 10;

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
///
/// Each hop is resolved, checked against the egress policy, and pinned to the
/// vetted address before connecting; redirects are followed manually so every
/// hop gets the same treatment.
pub(crate) async fn fetch_url(url: &str) -> Result<FetchedUrl, EngineBuildError> {
    // The *same* resolved policy the RUN sandbox consults — so
    // `--rootless-net-allow` (and `UMF_ROOTLESS_NET_ALLOW`) govern `ADD`
    // egress identically, rather than `ADD` growing a second, divergent knob.
    let policy = umf_engine::rootless::egress_policy();

    let mut target = reqwest::Url::parse(url).map_err(|e| fetch_failed(url, &e.to_string()))?;
    let mut hops = 0usize;

    let mut response = loop {
        let client = vetted_client(url, &target, &policy).await?;
        let response = client
            .get(target.clone())
            .send()
            .await
            .map_err(|e| fetch_failed(url, &e.to_string()))?;

        let status = response.status();
        if status.is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(fetch_failed(
                    url,
                    &format!("exceeded {MAX_REDIRECTS} redirects"),
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| fetch_failed(url, &format!("HTTP {status} without a Location")))?;
            // Resolve relative to the current hop, as an HTTP client must.
            let next = target
                .join(location)
                .map_err(|e| fetch_failed(url, &format!("bad redirect target: {e}")))?;
            info!(from = %target, to = %next, "ADD url: following redirect");
            target = next;
            continue;
        }
        if !status.is_success() {
            return Err(fetch_failed(url, &format!("HTTP {status}")));
        }
        break response;
    };

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

/// Build a client whose DNS for `target`'s host is pinned to an address the
/// policy allows.
///
/// `original` is the recipe's URL, used only so diagnostics name what the
/// author wrote rather than an intermediate redirect they never typed.
async fn vetted_client(
    original: &str,
    target: &reqwest::Url,
    policy: &EgressPolicy,
) -> Result<reqwest::Client, EngineBuildError> {
    let host = target
        .host_str()
        .ok_or_else(|| fetch_failed(original, &format!("{target} has no host")))?;
    let port = target.port_or_known_default().unwrap_or(443);

    let allowed = resolve_allowed(original, target, host, port, policy).await?;

    let builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // Every hop is vetted by this function, so reqwest must not chase one
        // on its own — an automatic redirect would bypass the policy entirely.
        .redirect(reqwest::redirect::Policy::none())
        // Pin the vetted address so the connect cannot land somewhere the
        // check did not cover (DNS rebinding between check and connect).
        .resolve(host, std::net::SocketAddr::new(allowed, port));

    builder
        .build()
        .map_err(|e| fetch_failed(original, &e.to_string()))
}

/// Resolve `host` and return the first address the policy allows, or a typed
/// refusal naming the category that blocked it.
async fn resolve_allowed(
    original: &str,
    target: &reqwest::Url,
    host: &str,
    port: u16,
    policy: &EgressPolicy,
) -> Result<IpAddr, EngineBuildError> {
    // A literal address needs no DNS; classify it directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return policy.check(ip).map(|()| ip).map_err(|denied| {
            EngineBuildError::AddUrlEgressDenied {
                url: original.to_string(),
                target: target.to_string(),
                address: denied.ip.to_string(),
                category: denied.category.to_string(),
            }
        });
    }

    let resolved: Vec<IpAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| fetch_failed(original, &format!("resolving {host}: {e}")))?
        .map(|sa| sa.ip())
        .collect();
    if resolved.is_empty() {
        return Err(fetch_failed(
            original,
            &format!("{host} resolved to no addresses"),
        ));
    }

    let allowed = policy.filter_resolved(&resolved);
    if let Some(&ip) = allowed.first() {
        if allowed.len() < resolved.len() {
            // A name that mixes public and host-internal addresses is a
            // classic rebinding shape; proceed on the public one but say so.
            warn!(
                host,
                resolved = resolved.len(),
                allowed = allowed.len(),
                "ADD url: some resolved addresses are policy-denied",
            );
        }
        return Ok(ip);
    }

    // Every address was denied: report the first one's category, which is the
    // actionable detail (which guard tripped), not just "denied".
    let denied = resolved
        .first()
        .and_then(|&ip| policy.check(ip).err())
        .map(|d| (d.ip.to_string(), d.category.to_string()))
        .unwrap_or_else(|| (host.to_string(), "host-internal".to_string()));
    Err(EngineBuildError::AddUrlEgressDenied {
        url: original.to_string(),
        target: target.to_string(),
        address: denied.0,
        category: denied.1,
    })
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
