//! Unit tests for the `engine_build` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use tempfile::TempDir;
use umf_parser::parse;

#[tokio::test]
async fn build_single_stage_rejects_multi_stage_input() {
    let src = "FROM alpine:3.21\nRUN echo a\n\nFROM scratch\nRUN echo b\n";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let err = build_single_stage(
        &layout,
        std::path::Path::new("."),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, EngineBuildError::MultiStageNotSupported),
        "expected MultiStageNotSupported, got {err:?}",
    );
}

#[tokio::test]
async fn add_from_unknown_stage_is_rejected() {
    // ADD --from=builder references nothing.
    let src = "FROM alpine:3.21\nADD --from=builder /a /b\n";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let err = build(
        &layout,
        std::path::Path::new("."),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .unwrap_err();
    match err {
        EngineBuildError::AddFromUnknownStage { stage } => {
            assert_eq!(stage, "builder");
        }
        other => panic!("expected AddFromUnknownStage, got {other:?}"),
    }
}

#[tokio::test]
async fn add_from_forward_reference_is_rejected() {
    // Stage 1 (unnamed) references "builder" — which is declared LATER
    // as the second stage. Should error before any execution.
    let src = "\
FROM alpine:3.21
ADD --from=builder /work/output /usr/local/bin/app

FROM debian:bookworm AS builder
RUN make
";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let err = build(
        &layout,
        std::path::Path::new("."),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .unwrap_err();
    match err {
        EngineBuildError::AddFromForwardReference { stage } => {
            assert_eq!(stage, "builder");
        }
        other => panic!("expected AddFromForwardReference, got {other:?}"),
    }
}

#[tokio::test]
async fn add_from_self_reference_is_rejected() {
    let src = "\
FROM debian:bookworm AS builder
ADD --from=builder /work/output /usr/local/bin/app
";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let err = build(
        &layout,
        std::path::Path::new("."),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .unwrap_err();
    match err {
        EngineBuildError::AddFromSelf { stage } => {
            assert_eq!(stage, "builder");
        }
        other => panic!("expected AddFromSelf, got {other:?}"),
    }
}

#[tokio::test]
async fn from_scratch_builds_an_image_from_nothing() {
    // The classic static-appliance shape: an empty base, files ADDed in,
    // metadata on top. No base image is pulled, no container is entered
    // (ADD synthesizes its upper directly), so this runs unprivileged.
    let context = TempDir::new().unwrap();
    std::fs::write(context.path().join("hello.txt"), b"hi from scratch\n").unwrap();

    let src = "FROM scratch\nADD hello.txt /hello.txt\nLABEL org.example.k=v\nENTRYPOINT [\"/hello.txt\"]\n";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let entry = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/scratch-app:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect("FROM scratch build succeeds");

    // One layer: the ADD. Config carries the label + entrypoint and the
    // container umf type.
    let manifest: oci_client::manifest::OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).unwrap()).unwrap();
    assert_eq!(manifest.layers.len(), 1, "exactly the ADD layer");
    let config: serde_json::Value =
        serde_json::from_slice(&layout.read_blob(&manifest.config.digest).unwrap()).unwrap();
    assert_eq!(
        config
            .pointer("/config/Labels/org.example.k")
            .and_then(|v| v.as_str()),
        Some("v"),
    );
    assert_eq!(
        config
            .pointer("/config/Labels/org.imagilux.umf.type")
            .and_then(|v| v.as_str()),
        Some("container"),
    );
    assert_eq!(
        config
            .pointer("/config/Entrypoint/0")
            .and_then(|v| v.as_str()),
        Some("/hello.txt"),
    );
    assert_eq!(
        config
            .pointer("/rootfs/diff_ids")
            .and_then(|v| v.as_array())
            .map(Vec::len),
        Some(1),
    );
}

#[tokio::test]
async fn add_local_substitutes_destination_but_history_keeps_original() {
    // `${VAR}` in an ADD destination is substituted for the *placement* (and
    // folded into the cache key), while the image history keeps the original
    // `${DEST}` text — no `--build-arg` value leaks into a layer's `created_by`.
    use umf_engine::bundle::{Bundle, BundleOptions};

    let context = TempDir::new().unwrap();
    std::fs::write(context.path().join("hello.txt"), b"hi\n").unwrap();

    let src = "ARG DEST=/opt\nFROM scratch\nADD hello.txt ${DEST}/\n";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();

    let mut build_args = std::collections::BTreeMap::new();
    build_args.insert("DEST".to_string(), "/srv".to_string());
    let options = EngineBuildOptions {
        build_args,
        ..EngineBuildOptions::default()
    };
    let entry = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/arg-add:1",
        &options,
    )
    .await
    .expect("ADD with a substituted destination builds");

    // The file lands at the substituted destination, not the default.
    let built = Bundle::from_image(
        &layout,
        "example.invalid/arg-add:1",
        &BundleOptions::default(),
    )
    .expect("bundle the built image");
    assert!(
        built.rootfs().join("srv/hello.txt").is_file(),
        "file should land at the --build-arg destination /srv",
    );
    assert!(
        !built.rootfs().join("opt").exists(),
        "the declared default /opt must not be used once overridden",
    );

    // History keeps the original `${DEST}` text; no `/srv` leaks into any line.
    let manifest: oci_client::manifest::OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).unwrap()).unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&layout.read_blob(&manifest.config.digest).unwrap()).unwrap();
    let history: Vec<String> = config
        .pointer("/history")
        .and_then(|h| h.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.pointer("/created_by").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    assert!(
        history.iter().any(|l| l == "ADD hello.txt ${DEST}/"),
        "history should keep the original placeholder, got {history:?}",
    );
    assert!(
        history.iter().all(|l| !l.contains("/srv")),
        "no ARG value may leak into history, got {history:?}",
    );
}

#[tokio::test]
async fn add_arg_expanding_to_traversal_is_rejected() {
    // Containment runs on the *substituted* path: an ARG whose value carries a
    // `..` must not smuggle a traversal past the guard (security).
    let context = TempDir::new().unwrap();
    std::fs::write(context.path().join("hello.txt"), b"hi\n").unwrap();
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();

    // Destination side: `${EVIL}/x` → `../../etc/x`.
    let dst_ast = parse("ARG EVIL=../../etc\nFROM scratch\nADD hello.txt ${EVIL}/x\n").unwrap();
    let err = build_single_stage(
        &layout,
        context.path(),
        &dst_ast,
        "example.invalid/evil-dst:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect_err("a destination that expands to `..` must be rejected");
    assert!(
        matches!(
            err,
            EngineBuildError::AddPathTraversal {
                kind: "destination",
                ..
            }
        ),
        "expected a destination traversal rejection, got {err:?}",
    );

    // Source side: `${EVIL}/passwd` → `../../../etc/passwd` (read host files).
    let src_ast = parse("ARG EVIL=../../../etc\nFROM scratch\nADD ${EVIL}/passwd /loot\n").unwrap();
    let err = build_single_stage(
        &layout,
        context.path(),
        &src_ast,
        "example.invalid/evil-src:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect_err("a source that expands to `..` must be rejected");
    assert!(
        matches!(
            err,
            EngineBuildError::AddPathTraversal { kind: "source", .. }
        ),
        "expected a source traversal rejection, got {err:?}",
    );
}

#[tokio::test]
async fn workdir_creates_directory_and_resolves_relative() {
    // Docker-faithful WORKDIR: `/opt/app` is created (so a later step can use
    // it) and a following relative `sub` resolves to `/opt/app/sub`. WORKDIR
    // synthesizes upper-dir layers (no RUN), so this runs unprivileged.
    use umf_engine::bundle::{Bundle, BundleOptions};

    let context = TempDir::new().unwrap();
    let src = "FROM scratch\nWORKDIR /opt/app\nWORKDIR sub\n";
    let ast = parse(src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let entry = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/workdir:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect("WORKDIR build succeeds");

    // The relative WORKDIR resolved against the previous one.
    let manifest: oci_client::manifest::OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).unwrap()).unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&layout.read_blob(&manifest.config.digest).unwrap()).unwrap();
    assert_eq!(
        config
            .pointer("/config/WorkingDir")
            .and_then(|v| v.as_str()),
        Some("/opt/app/sub"),
    );

    // Both directory levels were created in the image.
    let built = Bundle::from_image(
        &layout,
        "example.invalid/workdir:1",
        &BundleOptions::default(),
    )
    .expect("bundle the built image");
    assert!(
        built.rootfs().join("opt/app/sub").is_dir(),
        "WORKDIR should have created /opt/app/sub",
    );
}

#[tokio::test]
async fn add_oci_image_lays_rootfs_contents_at_destination() {
    use umf_engine::bundle::{Bundle, BundleOptions};
    use umf_oci::image::{ImageConfig, LayerSource, emit_image};

    // Seed the layout with a tiny "userland" image under its canonical
    // ref, so the stage pre-pull short-circuits to the cache — the whole
    // test runs network-free and unprivileged.
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let base_src = TempDir::new().unwrap();
    std::fs::create_dir_all(base_src.path().join("etc")).unwrap();
    std::fs::create_dir_all(base_src.path().join("usr/bin")).unwrap();
    std::fs::write(base_src.path().join("etc/os-release"), b"NAME=tiny\n").unwrap();
    std::fs::write(base_src.path().join("usr/bin/tool"), b"#!/bin/sh\n").unwrap();
    let layer = LayerSource::from_directory(base_src.path()).unwrap();
    emit_image(
        &layout,
        std::slice::from_ref(&layer),
        &ImageConfig::default(),
        "example.invalid/userland:1",
    )
    .expect("emit seed image");

    // `ADD <oci-ref> /` lays the image's rootfs *contents* on the root
    // tree; `ADD <oci-ref> /opt/base` nests them under the directory.
    let src = "FROM scratch\n\
               ADD example.invalid/userland:1 /\n\
               ADD example.invalid/userland:1 /opt/base\n\
               LABEL org.example.k=v\n";
    let ast = parse(src).expect("parse");
    let context = TempDir::new().unwrap();
    let entry = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/composed:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect("ADD <oci-ref> container build succeeds");

    // Two ADD layers on a scratch base.
    let manifest: oci_client::manifest::OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).unwrap()).unwrap();
    assert_eq!(manifest.layers.len(), 2, "one layer per ADD");

    // Materialise the built image and assert both placements: contents at
    // `/` (not nested under a `rootfs/` leaf) and contents under the
    // explicit directory.
    let built = Bundle::from_image(
        &layout,
        "example.invalid/composed:1",
        &BundleOptions::default(),
    )
    .expect("bundle the built image");
    assert!(built.rootfs().join("etc/os-release").is_file());
    assert!(built.rootfs().join("usr/bin/tool").is_file());
    assert!(
        !built.rootfs().join("rootfs").exists(),
        "no rootfs/ nesting"
    );
    assert!(built.rootfs().join("opt/base/etc/os-release").is_file());
    assert_eq!(
        std::fs::read(built.rootfs().join("etc/os-release")).unwrap(),
        b"NAME=tiny\n",
    );
}

#[tokio::test]
async fn add_oci_image_with_traversal_destination_is_rejected() {
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    // Seed so the pre-pull succeeds and the directive handler is reached.
    umf_oci::image::emit_image(
        &layout,
        &[],
        &umf_oci::image::ImageConfig::default(),
        "example.invalid/userland:1",
    )
    .expect("emit seed image");

    let src = "FROM scratch\nADD example.invalid/userland:1 /opt/../../escape\n";
    let ast = parse(src).expect("parse");
    let context = TempDir::new().unwrap();
    let err = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect_err("traversal destination must be rejected");
    assert!(
        format!("{err}").contains("destination"),
        "expected a traversal rejection, got {err:?}",
    );
}

/// Minimal HTTP/1.1 fixture server: serves `payload` with `status` for
/// every request until the test process exits. Std-only, no shell.
fn spawn_http(status: &'static str, payload: Vec<u8>) -> String {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            // Drain the request head; the fixtures never need the body.
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len(),
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&payload);
        }
    });
    format!("http://{addr}")
}

/// A one-file tar.gz built in memory.
fn targz_payload(path_in_tar: &str, contents: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path_in_tar, contents)
            .unwrap();
        builder.finish().unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).unwrap();
    gz.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_url_extracts_archives_and_places_files() {
    use umf_engine::bundle::{Bundle, BundleOptions};

    let archive_url = spawn_http(
        "200 OK",
        targz_payload("etc/payload.conf", b"from-archive\n"),
    );
    let file_url = spawn_http("200 OK", b"plain payload\n".to_vec());

    let src = format!(
        "FROM scratch\n\
         ADD {archive_url}/dist/bundle.tar.gz /\n\
         ADD {file_url}/notes.txt /docs/\n\
         ADD {file_url}/anything.bin /exact-name\n"
    );
    let ast = parse(&src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let context = TempDir::new().unwrap();
    let entry = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/from-urls:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect("ADD <url> build succeeds");

    let manifest: oci_client::manifest::OciImageManifest =
        serde_json::from_slice(&layout.read_blob(&entry.digest).unwrap()).unwrap();
    assert_eq!(manifest.layers.len(), 3, "one layer per ADD");

    let built = Bundle::from_image(
        &layout,
        "example.invalid/from-urls:1",
        &BundleOptions::default(),
    )
    .expect("bundle the built image");
    // tar.gz sniffed and extracted: contents at /, no leaf nesting.
    assert_eq!(
        std::fs::read(built.rootfs().join("etc/payload.conf")).unwrap(),
        b"from-archive\n",
    );
    // Plain file + trailing-slash dst: the URL leaf names the file.
    assert_eq!(
        std::fs::read(built.rootfs().join("docs/notes.txt")).unwrap(),
        b"plain payload\n",
    );
    // Plain file + explicit dst: dst names the file.
    assert_eq!(
        std::fs::read(built.rootfs().join("exact-name")).unwrap(),
        b"plain payload\n",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_url_fetch_failure_is_a_clear_error() {
    let gone = spawn_http("404 Not Found", b"nope".to_vec());
    let src = format!("FROM scratch\nADD {gone}/missing.tar.gz /\n");
    let ast = parse(&src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let context = TempDir::new().unwrap();
    let err = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect_err("a 404 must fail the build");
    assert!(
        matches!(err, EngineBuildError::AddUrlFetchFailed { .. }),
        "expected AddUrlFetchFailed, got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_url_unsupported_archive_is_a_clear_error() {
    // A zstd frame: sniffable, but not extractable yet.
    let zstd_magic = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00];
    let url = spawn_http("200 OK", zstd_magic);
    let src = format!("FROM scratch\nADD {url}/payload.tar.zst /\n");
    let ast = parse(&src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let context = TempDir::new().unwrap();
    let err = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect_err("an unsupported archive must fail clearly");
    assert!(
        matches!(err, EngineBuildError::AddUrlArchiveUnsupported { .. }),
        "expected AddUrlArchiveUnsupported, got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_url_gzip_that_is_not_a_tar_is_a_clear_error() {
    use std::io::Write as _;
    // A gzip stream wrapping a plain string, not a tar. The gzip magic marks
    // it for extraction, so it must fail with a clear extract error rather
    // than an opaque staging failure or a silent empty unpack.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(b"i am not a tar, just a gzipped string\n")
        .unwrap();
    let payload = gz.finish().unwrap();
    let url = spawn_http("200 OK", payload);

    let src = format!("FROM scratch\nADD {url}/data.gz /opt/\n");
    let ast = parse(&src).expect("parse");
    let layout_dir = TempDir::new().unwrap();
    let layout = ImageLayout::init(layout_dir.path()).unwrap();
    let context = TempDir::new().unwrap();
    let err = build_single_stage(
        &layout,
        context.path(),
        &ast,
        "example.invalid/x:1",
        &EngineBuildOptions::default(),
    )
    .await
    .expect_err("a gzipped non-tar must fail clearly");
    assert!(
        matches!(err, EngineBuildError::AddUrlExtractFailed { .. }),
        "expected AddUrlExtractFailed, got {err:?}",
    );
}
