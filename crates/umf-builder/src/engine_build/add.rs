//! The `ADD` directive handlers and their path helpers.
//!
//! Covers all four `ADD` source shapes — a local context path, a cross-stage
//! `--from=<stage>` reference, a remote `<url>`, and a bare `<oci-ref>` — plus
//! the containment guard and destination-resolution helpers they share. Each
//! handler synthesises one layer from a staged upper-dir and folds it into the
//! step cache. Split out of [`super::directives`] to keep that module focused
//! on dispatch and the metadata handlers.
//!
//! `COPY` lowers through this same module: it sets [`Add::plain_copy`], which
//! restricts it to the local-path and `--from` shapes — the remote (URL / OCI)
//! shapes are rejected up front (see [`plain_copy_rejected_kind`]).

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tracing::info;
use umf_core::ast::{Add, AddSource};
use umf_engine::bundle::{Bundle, BundleOptions, LayerStrategy};
use umf_engine::overlay::PersistedUpper;
use umf_oci::registry::error::RegistryError;

use super::EngineBuildError;
use super::cache::{
    StepCache, add_cache_key, add_source_digest, cross_stage_add_cache_key, layer_from_cache,
    oci_image_add_cache_key, parent_state_hash, url_add_cache_key,
};
use super::directives::{BuildCtx, store_layer_cache};
use super::fetch::url_leaf;
use super::state::BuildState;

/// For a `COPY` (plain-copy) directive, decide whether `source` is a remote
/// kind that `COPY` must refuse. Returns the human-readable kind (for the
/// error message) when the source is a URL or OCI image reference, or `None`
/// for a local-context / `--from=<stage>` path, which `COPY` accepts.
fn plain_copy_rejected_kind(source: &AddSource) -> Option<&'static str> {
    match source {
        AddSource::Url(_) => Some("a URL"),
        AddSource::Oci(_) => Some("an OCI image reference"),
        AddSource::Path(_) => None,
    }
}

pub(super) fn apply_add(
    ctx: &BuildCtx,
    state: &mut BuildState,
    add: &Add,
    lookup_cache: bool,
) -> Result<(), EngineBuildError> {
    let &BuildCtx {
        context_dir,
        layout,
        cache,
        architecture,
        ..
    } = ctx;
    // `COPY` is a plain local/stage copy: a remote source (URL / OCI image)
    // is `ADD`'s job, so reject it before any fetch happens. Checked here
    // rather than at parse time so the message names what the source resolved
    // to (URL vs OCI image reference).
    if add.plain_copy
        && let Some(kind) = plain_copy_rejected_kind(&add.source)
    {
        return Err(EngineBuildError::CopyRemoteSource {
            reference: add.source.as_str().to_string(),
            kind,
        });
    }
    // `${VAR}` / `$VAR` substitution: every operand is expanded
    // against the stage's `ARG` scope for the *operation* (the path joined, the
    // ref pulled, the layer-cache key), while the image history keeps the
    // original `${VAR}` text — so an ARG value never lands in a layer's
    // `created_by`. The URL / OCI handlers do their own substitution internally
    // (the raw operands ride in for their history line).
    let raw_src = match &add.source {
        AddSource::Path(spanned) => spanned.value.as_str(),
        AddSource::Url(spanned) => {
            return apply_add_url(
                ctx,
                state,
                spanned.value.as_str(),
                add.destination.value.as_str(),
                lookup_cache,
            );
        }
        AddSource::Oci(spanned) => {
            return apply_add_oci_image(
                ctx,
                state,
                spanned.value.as_str(),
                add.destination.value.as_str(),
                lookup_cache,
            );
        }
    };
    let raw_dst = add.destination.value.as_str();
    let src_path_str = state.subst(raw_src);
    let dst = state.subst(raw_dst);
    // History verb: `COPY` and `ADD` share this lowering and (for identical
    // content + dest) the same cache entry; the verb only labels the history.
    let verb = if add.plain_copy { "COPY" } else { "ADD" };

    // Containment: every ADD path is resolved relative to a root — the
    // build context, a producing stage's rootfs, or the consumer's upper-dir.
    // A `..` component lets `Path::join` climb out of that root and read host
    // files into a layer or write outside the image, so reject it on both the
    // source and destination before any join. The guard runs on the
    // *substituted* paths — a `${VAR}` that expands to a `..` component must not
    // slip past it. This single check guards both the local and cross-stage
    // paths below.
    reject_traversal("source", &src_path_str)?;
    reject_traversal("destination", &dst)?;

    // Cross-stage `ADD --from=<stage>` resolves against the producing stage's
    // rootfs (materialised from its OCI image in the layout); a local source
    // resolves against `context_dir`.
    if let Some(from) = add.from.as_ref() {
        let history_line = format!("{verb} --from={} {raw_src} {raw_dst}", from.value.as_str());
        return apply_add_from_stage(
            ctx,
            state,
            &src_path_str,
            &dst,
            from.value.as_str(),
            lookup_cache,
            history_line,
        );
    }

    let history_line = format!("{verb} {raw_src} {raw_dst}");

    // Resolve source relative to context_dir. Strip a leading `/` first:
    // `Path::join` with an absolute path discards `context_dir`, so an absolute
    // ADD source would otherwise read straight from the host root. The
    // cross-stage handler strips the same way; `..` is already rejected above.
    let src_abs = context_dir.join(src_path_str.trim_start_matches('/'));
    if !src_abs.exists() {
        return Err(EngineBuildError::AddSourceNotFound {
            path: src_path_str.clone(),
            context: context_dir.display().to_string(),
        });
    }

    // Cache lookup. Source-content digest folds in every file's bytes
    // and path, so any change busts the cache; the destination folds in the
    // substituted value, so a changed ARG rebuilds.
    let src_digest = add_source_digest(&src_abs)?;
    let parent = parent_state_hash(state);
    let key = add_cache_key(
        architecture.oci_arch_string(),
        state.compression.media_type(),
        &parent,
        &src_digest,
        &dst,
    );
    if lookup_cache
        && let Some(entry) = cache.lookup(&key)
        && let Some(reused) = layer_from_cache(layout, &entry)?
    {
        info!("engine build: ADD cache hit (skipping copy)");
        state.adopt_cached_layer(reused, entry.history_line);
        return Ok(());
    }

    // Stage the source into a synthetic upper-dir mirroring overlayfs's output
    // and package it as a layer (shared tail with `ADD --from`).
    stage_copy_layer(state, cache, &key, &src_abs, &dst, history_line)
}

/// Handle `ADD --from=<stage> /src /dst`. Materialise the producing
/// stage's rootfs (from its in-layout OCI image), copy the requested
/// path out of it into a fresh upper-dir, and package as a layer.
///
/// Cache key folds in the producing stage's manifest digest plus the
/// requested source path inside it — so changes to the upstream stage
/// invalidate downstream consumers naturally.
fn apply_add_from_stage(
    ctx: &BuildCtx,
    state: &mut BuildState,
    src_path_str: &str,
    dst: &str,
    from_stage: &str,
    lookup_cache: bool,
    history_line: String,
) -> Result<(), EngineBuildError> {
    let &BuildCtx {
        layout,
        cache,
        produced,
        architecture,
        ..
    } = ctx;
    // `src_path_str` / `dst` arrive already `${VAR}`-substituted (the operands
    // the copy + cache key use); `history_line` carries the original `${VAR}`
    // text the caller built, so no ARG value leaks into the image history.

    let producer_ref =
        produced
            .get(from_stage)
            .ok_or_else(|| EngineBuildError::AddFromUnknownStage {
                stage: from_stage.to_string(),
            })?;

    let producer_manifest_digest = layout
        .lookup_ref(producer_ref)?
        .ok_or_else(|| RegistryError::NotFound(producer_ref.clone()))?
        .digest;

    // Cache lookup keyed on the producer's manifest digest plus the
    // src path inside it. A no-change rebuild reuses the existing
    // layer blob.
    let parent = parent_state_hash(state);
    let key = cross_stage_add_cache_key(
        state.compression.media_type(),
        &parent,
        &producer_manifest_digest,
        src_path_str,
        dst,
    );
    if lookup_cache
        && let Some(entry) = cache.lookup(&key)
        && let Some(reused) = layer_from_cache(layout, &entry)?
    {
        info!("engine build: ADD --from cache hit (skipping copy)");
        state.adopt_cached_layer(reused, entry.history_line);
        return Ok(());
    }

    // Materialise the producer's rootfs in a tempdir so we can copy
    // the requested path out of it. Bundle::from_image handles the
    // OCI layer unpacking (with whiteouts). The Bundle's TempDir
    // cleans up when we drop it at end of scope.
    // Cross-stage ADD reads files straight out of `rootfs()`, so it needs
    // the merged tree, not the erofs lower stack.
    let bundle_opts = BundleOptions::for_host("umf-build-xref", LayerStrategy::Merge, architecture);
    let producer_bundle = Bundle::from_image(layout, producer_ref, &bundle_opts)?;

    // The producer's rootfs is at `producer_bundle.rootfs()`. Resolve
    // `src_path_str` relative to it.
    let src_rel = src_path_str.trim_start_matches('/');
    let src_abs = producer_bundle.rootfs().join(src_rel);
    if !src_abs.exists() {
        return Err(EngineBuildError::AddSourceNotFound {
            path: src_path_str.to_string(),
            context: format!("stage `{from_stage}` rootfs"),
        });
    }

    stage_copy_layer(state, cache, &key, &src_abs, dst, history_line)
}

/// Handle `ADD <url> <dst>`: consume the payload fetched up front by
/// [`super::build`]'s async phase, sniff it by magic number (the spec's
/// fingerprinting contract — extensions are never trusted), and either
/// extract it or place it as a file:
///
/// - **tar / tar.gz** — extracted so the *contents* land at `dst` (always
///   a directory), via [`BuildStaging`]'s traversal-contained unpack.
/// - **xz / bzip2 / zstd / squashfs** — a clear "not extracted yet" error
///   rather than a silent file placement of something the recipe author
///   expected to be unpacked.
/// - **anything else** — a plain file at `dst`, with docker's
///   trailing-slash rule (the URL's leaf name lands inside a `dst/`).
///
/// The layer cache keys on the payload's sha256 — computed during the
/// fetch — so an unchanged remote is a cache hit even though it is
/// re-downloaded every build (docker semantics), and a silently-changed
/// remote busts the cache.
fn apply_add_url(
    ctx: &BuildCtx,
    state: &mut BuildState,
    url_raw: &str,
    dst_raw: &str,
    lookup_cache: bool,
) -> Result<(), EngineBuildError> {
    let &BuildCtx {
        layout,
        cache,
        fetched_urls,
        ..
    } = ctx;
    // History keeps the original `${VAR}` text; the fetch lookup + cache key use
    // the substituted URL / destination. The pre-fetch pass substitutes the URL
    // the same way, so `fetched_urls` is keyed on the resolved URL we look up
    // here.
    let history_line = format!("ADD {url_raw} {dst_raw}");
    let url_s = state.subst(url_raw);
    let dst_s = state.subst(dst_raw);
    let url = url_s.as_str();
    let dst = dst_s.as_str();
    reject_traversal("destination", dst)?;

    let fetched = fetched_urls.get(url).ok_or_else(|| {
        // Unreachable by construction — build() fetches every URL source
        // before walking directives — but a clear error beats a panic.
        EngineBuildError::AddUrlFetchFailed {
            url: url.to_string(),
            detail: "payload was not staged by the fetch phase".to_string(),
        }
    })?;

    let parent = parent_state_hash(state);
    let key = url_add_cache_key(
        state.compression.media_type(),
        &parent,
        &fetched.digest,
        dst,
    );
    if lookup_cache
        && let Some(cached) = cache.lookup(&key)
        && let Some(reused) = layer_from_cache(layout, &cached)?
    {
        info!("engine build: ADD <url> cache hit (skipping unpack)");
        state.adopt_cached_layer(reused, cached.history_line);
        return Ok(());
    }

    // Sniff the payload by magic number, never by extension — reading only
    // a short prefix. The body stays on disk: the fetch streamed it there
    // so a multi-gigabyte source never sits in memory, and the consume side
    // keeps that promise (peek to classify, then stream to extract or copy,
    // never a whole-file read).
    let format = sniff_format(fetched.file.path())?;

    let upper_holder = TempDir::new()?;
    let upper_root = upper_holder.path().join("upper");
    std::fs::create_dir_all(&upper_root)?;

    use umf_oci::format::Format;
    match format {
        Format::Tar | Format::Gzip => {
            // Extract through the staging machinery, streaming straight from
            // the staged file: it decodes gzip or plain tar and contains
            // path traversal, the same guarantees the bootable userland
            // unpack already relies on. A gzip/tar source that is not
            // actually a (optionally gzipped) tar — a lone `.gz`, a corrupt
            // archive — surfaces as a clear extract error.
            let mut staging = umf_oci::staging::BuildStaging::new()?;
            staging.unpack_tarball(fetched.file.path()).map_err(|e| {
                EngineBuildError::AddUrlExtractFailed {
                    url: url.to_string(),
                    format: format.as_str().to_string(),
                    detail: e.to_string(),
                }
            })?;
            let dst_dir = path_within_upper(&upper_root, dst);
            std::fs::create_dir_all(&dst_dir)?;
            crate::fsutil::copy_dir_recursive(staging.path(), &dst_dir)?;
        }
        Format::Zstd | Format::Xz | Format::Bzip2 | Format::Squashfs => {
            return Err(EngineBuildError::AddUrlArchiveUnsupported {
                url: url.to_string(),
                format: format.as_str().to_string(),
            });
        }
        Format::Unknown => {
            // A plain file. Trailing-slash rule: `/dst/` takes the URL's
            // leaf name; otherwise `dst` names the file itself. Copied
            // straight from the staged file, never read into memory.
            let dst_inside = if dst.ends_with('/') {
                format!("{dst}{}", url_leaf(url))
            } else {
                dst.to_string()
            };
            reject_traversal("destination", &dst_inside)?;
            let dst_in_upper = path_within_upper(&upper_root, &dst_inside);
            if let Some(parent) = dst_in_upper.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(fetched.file.path(), &dst_in_upper)?;
        }
    }

    package_and_cache_upper(state, cache, &key, upper_holder, upper_root, history_line)
}

/// Classify a staged `ADD <url>` payload by reading only its leading bytes.
/// `format::detect` needs at most the first 512 (the `ustar` magic sits at
/// offset 257), so the body stays on disk — a multi-gigabyte source is never
/// read into memory just to be classified.
fn sniff_format(path: &Path) -> Result<umf_oci::format::Format, EngineBuildError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 512];
    let mut filled = 0;
    while filled < head.len() {
        let n = file.read(&mut head[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(umf_oci::format::detect(&head[..filled]))
}

/// Handle a bare `ADD <oci-ref> <dst>` on the container target: lay the
/// external image's merged rootfs **contents** at `dst`, packaged as one
/// layer — the same base-userland semantics the bootable target gives the
/// directive, so a recipe means one thing regardless of shape.
///
/// `dst` is always treated as a directory: `ADD imagilux/rootfs:7 /` lays
/// a userland on the root tree; it never nests the source under a leaf
/// directory the way a local directory ADD would. The image itself was
/// pulled into the layout by the stage pre-pass (the directive walk is
/// synchronous); resolution here re-canonicalizes the reference the same
/// way, so the lookup key agrees. Platform selection and whiteout
/// application come from [`Bundle::from_image`]'s merged unpack.
fn apply_add_oci_image(
    ctx: &BuildCtx,
    state: &mut BuildState,
    image_ref_raw: &str,
    dst_raw: &str,
    lookup_cache: bool,
) -> Result<(), EngineBuildError> {
    let &BuildCtx {
        layout,
        cache,
        architecture,
        ..
    } = ctx;
    // History keeps the original `${VAR}` text; the pull + cache key use the
    // substituted reference / destination. The stage's pre-pull pass substitutes
    // the reference the same way, so the image is already in the layout under the
    // resolved ref we re-canonicalize and look up here.
    let history_line = format!("ADD {image_ref_raw} {dst_raw}");
    let image_ref_s = state.subst(image_ref_raw);
    let dst_s = state.subst(dst_raw);
    let image_ref = image_ref_s.as_str();
    let dst = dst_s.as_str();
    reject_traversal("destination", dst)?;

    let reference: oci_client::Reference =
        image_ref.parse().map_err(|e: oci_client::ParseError| {
            EngineBuildError::InvalidReference(image_ref.to_string(), e.to_string())
        })?;
    let canonical = reference.whole();
    let entry = layout.lookup_ref(&canonical)?.ok_or_else(|| {
        EngineBuildError::Registry(umf_oci::registry::RegistryError::NotFound(
            canonical.clone(),
        ))
    })?;

    // Cache lookup, keyed on the resolved manifest digest — a retagged
    // upstream busts the cache through the digest, not the tag.
    let parent = parent_state_hash(state);
    let key = oci_image_add_cache_key(state.compression.media_type(), &parent, &entry.digest, dst);
    if lookup_cache
        && let Some(cached) = cache.lookup(&key)
        && let Some(reused) = layer_from_cache(layout, &cached)?
    {
        info!("engine build: ADD <oci-ref> cache hit (skipping unpack)");
        state.adopt_cached_layer(reused, cached.history_line);
        return Ok(());
    }

    // Materialise the image's merged rootfs (whiteouts applied, platform
    // selected) and copy its contents at `dst` inside a fresh upper-dir.
    let bundle_opts = BundleOptions::for_host("umf-build-add", LayerStrategy::Merge, architecture);
    let source_bundle = Bundle::from_image(layout, &canonical, &bundle_opts)?;

    let upper_holder = TempDir::new()?;
    let upper_root = upper_holder.path().join("upper");
    std::fs::create_dir_all(&upper_root)?;
    let dst_dir = path_within_upper(&upper_root, dst);
    crate::fsutil::copy_dir_recursive(source_bundle.rootfs(), &dst_dir)?;

    package_and_cache_upper(state, cache, &key, upper_holder, upper_root, history_line)
}

/// Stage `src_abs` (a file or directory) into a fresh upper-dir at the recipe
/// destination, then package it as a layer and cache it. The shared tail of
/// the local `ADD` and `ADD --from=<stage>` handlers: both resolve a
/// source path against some root, then copy it in identically.
fn stage_copy_layer(
    state: &mut BuildState,
    cache: &StepCache,
    key: &str,
    src_abs: &Path,
    dst: &str,
    history_line: String,
) -> Result<(), EngineBuildError> {
    // Stage a synthetic upper-dir mirroring overlayfs's output, so the
    // packaging machinery (`LayerSource::from_directory`) can pick it up the
    // same way it picks up a RUN's upper.
    let upper_holder = TempDir::new()?;
    let upper_root = upper_holder.path().join("upper");
    std::fs::create_dir_all(&upper_root)?;

    // Compute the destination *inside* the upper-dir. Recipe destinations
    // follow the trailing-slash rule: trailing `/` ⇒ directory; otherwise, if
    // src is a directory, dst is a directory; if src is a file and dst has no
    // trailing slash, dst is the file.
    let dst_inside = compute_add_destination(dst, src_abs);
    let dst_in_upper = path_within_upper(&upper_root, &dst_inside);
    if let Some(parent) = dst_in_upper.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if src_abs.is_dir() {
        crate::fsutil::copy_dir_recursive(src_abs, &dst_in_upper)?;
    } else {
        std::fs::copy(src_abs, &dst_in_upper)?;
    }

    package_and_cache_upper(state, cache, key, upper_holder, upper_root, history_line)
}

/// Package a staged upper-dir into a new layer and cache it — the shared tail
/// of every `ADD` handler.
fn package_and_cache_upper(
    state: &mut BuildState,
    cache: &StepCache,
    key: &str,
    upper_holder: TempDir,
    upper_root: PathBuf,
    history_line: String,
) -> Result<(), EngineBuildError> {
    // Wrap the synthesised upper-dir in a PersistedUpper-shaped guard so it
    // integrates with the BuildState's other upper guards.
    let persisted = PersistedUpper::from_owned_tempdir(upper_holder, upper_root);
    let layer = state.push_new_layer(persisted, history_line.clone())?;
    store_layer_cache(cache, key, layer, history_line)
}

/// Reject an ADD source/destination that would escape its containment root.
///
/// UMF resolves every ADD path relative to a root (the build context, a
/// producing stage's rootfs, or the consumer's upper-dir). A `..` component
/// lets `Path::join` climb out of that root, which would let a recipe read
/// arbitrary host files into a layer (e.g. `ADD ../../etc/passwd /loot`) or
/// write outside the image (e.g. `ADD foo /../../etc/cron.d/x`). This is the
/// builder-side analogue of `umf-oci::materialize`'s `safe_descend` and
/// `umf-compile`'s `rootfs_subpath` containment guards.
fn reject_traversal(kind: &'static str, raw: &str) -> Result<(), EngineBuildError> {
    if Path::new(raw)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(EngineBuildError::AddPathTraversal {
            kind,
            path: raw.to_string(),
        });
    }
    Ok(())
}

fn compute_add_destination(dst: &str, src: &Path) -> String {
    // Trailing-slash + source-shape rules:
    //  - If `dst` ends with `/`, dst is always a directory; src lands inside.
    //  - If src is a directory, dst is a directory; recurse.
    //  - Otherwise dst is the destination filename.
    if dst.ends_with('/') {
        let leaf = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        format!("{dst}{leaf}")
    } else if src.is_dir() {
        format!("{dst}/")
    } else {
        dst.to_string()
    }
}

/// Resolve `dst_inside` (a path string, possibly leading-/, possibly
/// relative) against `upper_root`. Strips a leading `/` so absolute
/// recipe paths land relative to the upper-dir (which represents the
/// rootfs).
fn path_within_upper(upper_root: &Path, dst_inside: &str) -> PathBuf {
    let trimmed = dst_inside.trim_start_matches('/');
    upper_root.join(trimmed)
}

#[cfg(test)]
mod tests;
