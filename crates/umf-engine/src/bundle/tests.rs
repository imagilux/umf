//! Unit tests for the `bundle` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;

#[test]
fn supported_layer_media_types() {
    assert!(is_supported_layer_media_type(
        oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE
    ));
    assert!(is_supported_layer_media_type(
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
    ));
    // zstd layers are consumable: apply_layer fingerprints + decodes them.
    assert!(is_supported_layer_media_type(
        umf_oci::image::IMAGE_LAYER_ZSTD_MEDIA_TYPE
    ));
    assert!(!is_supported_layer_media_type(
        "application/vnd.oci.image.layer.v1.tar"
    ));
}

#[test]
fn erofs_encodable_excludes_zstd() {
    // The erofs lower path drives `mkfs.erofs --gzip`, so only gzip layers
    // are directly encodable; zstd is consumable (above) but routes through
    // the merged-unpack path instead of erofs.
    assert!(is_erofs_encodable_media_type(
        oci_client::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE
    ));
    assert!(is_erofs_encodable_media_type(
        "application/vnd.docker.image.rootfs.diff.tar.gzip"
    ));
    assert!(!is_erofs_encodable_media_type(
        umf_oci::image::IMAGE_LAYER_ZSTD_MEDIA_TYPE
    ));
}

#[test]
fn default_caps_excludes_dangerous_ones() {
    let caps = default_caps();
    assert!(!caps.contains(&Capability::SysAdmin));
    assert!(!caps.contains(&Capability::SysModule));
    assert!(!caps.contains(&Capability::SysTime));
}

#[test]
fn for_host_derives_rootless_from_process_identity() {
    // Regression: build call sites must derive the rootless shape
    // (user namespace + uid/gid mappings) from the running process, never
    // hardcode `rootless: false` / uid 0. Under an unprivileged build
    // (uid != 0) the options must request a user namespace, or libcontainer
    // rejects the container ("rootless container requires valid user
    // namespace definition").
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();

    let opts = BundleOptions::for_host(
        "umf-build",
        LayerStrategy::Merge,
        umf_core::architecture::Architecture::default(),
    );

    // for_host() does not enter a user namespace, so the spec requests one
    // exactly when we lack real host privilege (an unprivileged build
    // must get a user namespace, never a root-assuming spec).
    assert_eq!(opts.rootless, !opts.host_privileged);
    if !nix::unistd::geteuid().is_root() {
        assert!(
            opts.rootless,
            "unprivileged build must request a user namespace"
        );
        assert!(!opts.host_privileged);
    }
    assert_eq!(opts.host_uid, uid);
    assert_eq!(opts.host_gid, gid);
    // Caller-supplied fields are honoured, not clobbered by the default.
    assert_eq!(opts.hostname, "umf-build");
    assert!(matches!(opts.layer_strategy, LayerStrategy::Merge));
}

#[test]
fn runtime_spec_masks_paths_and_uses_fresh_sysfs() {
    let rootfs = std::path::PathBuf::from("/tmp/umf-spec-test-rootfs");
    let spec = build_runtime_spec(
        &rootfs,
        &ImageConfigDoc::default(),
        &BundleOptions::default(),
    )
    .expect("build runtime spec");
    let linux = spec.linux().as_ref().expect("linux section present");

    // Standard runc/crun masked + readonly paths must be present.
    let masked = linux.masked_paths().as_ref().expect("masked_paths set");
    assert!(masked.iter().any(|p| p == "/proc/kcore"));
    assert!(masked.iter().any(|p| p == "/sys/firmware"));
    let ro = linux.readonly_paths().as_ref().expect("readonly_paths set");
    assert!(ro.iter().any(|p| p == "/proc/sysrq-trigger"));

    // /sys must be a fresh sysfs, never an rbind of the host tree.
    let mounts = spec.mounts().as_ref().expect("mounts set");
    let sys = mounts
        .iter()
        .find(|m| m.destination().to_str() == Some("/sys"))
        .expect("/sys mount present");
    assert_eq!(sys.typ().as_deref(), Some("sysfs"));
    let has_rbind = sys
        .options()
        .as_ref()
        .is_some_and(|o| o.iter().any(|x| x == "rbind"));
    assert!(!has_rbind, "/sys must not be an rbind of the host /sys");
}

#[test]
fn runtime_spec_has_cgroup_namespace_and_root_resources() {
    // Cgroup namespace requested for every build.
    let spec = build_runtime_spec(
        std::path::Path::new("/tmp/umf-ns-test"),
        &ImageConfigDoc::default(),
        &BundleOptions::default(),
    )
    .expect("spec");
    let nss = spec
        .linux()
        .as_ref()
        .and_then(|l| l.namespaces().as_ref())
        .expect("namespaces");
    assert!(
        nss.iter().any(|n| n.typ() == LinuxNamespaceType::Cgroup),
        "cgroup namespace must be requested"
    );

    // Privileged (root) build: a pids resource cap is applied.
    let root_opts = BundleOptions {
        rootless: false,
        host_privileged: true,
        host_uid: 0,
        host_gid: 0,
        ..BundleOptions::default()
    };
    let root_spec = build_runtime_spec(
        std::path::Path::new("/tmp/umf-res-test"),
        &ImageConfigDoc::default(),
        &root_opts,
    )
    .expect("root spec");
    let pids = root_spec
        .linux()
        .as_ref()
        .and_then(|l| l.resources().as_ref())
        .and_then(|r| r.pids().as_ref())
        .expect("pids cap present for a root build");
    assert_eq!(pids.limit(), 4096);
}

#[test]
fn rootless_entered_userns_gets_pids_cap_but_legacy_fallback_does_not() {
    // A rootless build that entered its own user namespace
    // (`rootless: false`, not host-privileged) still carries the fork-bomb pids
    // cap — youki's systemd manager applies it as TasksMax in the delegated
    // scope. The legacy library-consumer fallback (`rootless: true`, where youki
    // creates the user namespace) does not, since we don't own its cgroup setup.
    let entered = BundleOptions {
        rootless: false,
        host_privileged: false,
        host_uid: 1000,
        host_gid: 1000,
        ..BundleOptions::default()
    };
    let pids_limit = |opts: &BundleOptions| {
        build_runtime_spec(
            std::path::Path::new("/tmp/umf-pids-377"),
            &ImageConfigDoc::default(),
            opts,
        )
        .expect("spec")
        .linux()
        .as_ref()
        .and_then(|l| l.resources().as_ref())
        .and_then(|r| r.pids().as_ref())
        .map(|p| p.limit())
    };

    assert_eq!(
        pids_limit(&entered),
        Some(4096),
        "entered-userns rootless build must carry the pids cap"
    );

    let legacy = BundleOptions {
        rootless: true,
        ..entered.clone()
    };
    assert_eq!(
        pids_limit(&legacy),
        None,
        "legacy rootless fallback must not set a pids cap"
    );
}

#[test]
fn runtime_spec_carries_default_seccomp_profile() {
    use oci_spec::runtime::LinuxSeccompAction;

    // The seccomp profile must be attached for both root and rootless
    // builds: deny-by-default + a non-empty syscall allowlist.
    for rootless in [false, true] {
        let opts = BundleOptions {
            rootless,
            host_uid: if rootless { 1000 } else { 0 },
            host_gid: if rootless { 1000 } else { 0 },
            ..BundleOptions::default()
        };
        let spec = build_runtime_spec(
            std::path::Path::new("/tmp/umf-seccomp-test"),
            &ImageConfigDoc::default(),
            &opts,
        )
        .expect("spec");
        let seccomp = spec
            .linux()
            .as_ref()
            .and_then(|l| l.seccomp().as_ref())
            .unwrap_or_else(|| panic!("seccomp profile must be set (rootless={rootless})"));
        assert_eq!(
            seccomp.default_action(),
            LinuxSeccompAction::ScmpActErrno,
            "default action must deny (errno) for rootless={rootless}",
        );
        let allowlist = seccomp
            .syscalls()
            .as_ref()
            .expect("syscall allowlist present");
        let total: usize = allowlist.iter().map(|b| b.names().len()).sum();
        assert!(
            total > 100,
            "allowlist must be non-empty/substantial, got {total} (rootless={rootless})",
        );
    }
}

#[test]
fn parse_subid_range_matches_username_then_uid() {
    let body = "# delegated ranges\nalice:100000:65536\nbob:200000:65536\n1000:300000:1000\n";
    assert_eq!(
        parse_subid_range(body, Some("bob"), 4242),
        Some(SubIdRange {
            start: 200_000,
            count: 65_536
        })
    );
    // Falls back to a numeric-uid match when the username doesn't appear.
    assert_eq!(
        parse_subid_range(body, Some("carol"), 1000),
        Some(SubIdRange {
            start: 300_000,
            count: 1000
        })
    );
    // No match; zero-count ranges are ignored.
    assert_eq!(parse_subid_range(body, Some("dave"), 5), None);
    assert_eq!(parse_subid_range("eve:1:0\n", Some("eve"), 5), None);
}

/// Build a tiny gzipped tar carrying one regular file at the requested
/// path. Sufficient for unit-testing the layer unpacker without a real
/// OCI image around.
fn synth_layer_with(file: &str, contents: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_path(file).expect("set tar path");
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, contents).expect("append tar entry");
        builder.finish().expect("finish tar");
    }
    let mut out = Vec::new();
    let mut enc = GzEncoder::new(&mut out, Compression::default());
    use std::io::Write as _;
    enc.write_all(&tar_bytes).expect("gz write");
    enc.finish().expect("gz finish");
    out
}

/// Like [`synth_layer_with`] but writes `name` *raw* into the tar
/// header, bypassing `Header::set_path` (which refuses `..`). Forges
/// the hostile entry a malicious layer would carry.
fn synth_layer_raw_name(name: &str, contents: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write as _;
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        // Stamp the path bytes straight into the 100-byte name field,
        // sidestepping `set_path`'s `..` guard, then fix the checksum.
        let raw = header.as_mut_bytes();
        let nb = name.as_bytes();
        raw[..nb.len()].copy_from_slice(nb);
        header.set_cksum();
        builder.append(&header, contents).expect("append raw entry");
        builder.finish().expect("finish tar");
    }
    let mut out = Vec::new();
    let mut enc = GzEncoder::new(&mut out, Compression::default());
    enc.write_all(&tar_bytes).expect("gz write");
    enc.finish().expect("gz finish");
    out
}

#[test]
fn unpacks_a_simple_layer() {
    let dst = TempDir::new().expect("tempdir");
    let blob = synth_layer_with("usr/local/bin/hi", b"echo hi\n");
    unpack_layer_into(&blob, dst.path()).expect("unpack");
    let contents = fs::read(dst.path().join("usr/local/bin/hi")).expect("read");
    assert_eq!(contents, b"echo hi\n");
}

/// A zstd-compressed layer (`+zstd`) unpacks through the same path: the
/// gate accepts it and `apply_layer` fingerprints + zstd-decodes the blob.
#[test]
fn unpacks_a_zstd_layer() {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_path("usr/local/bin/hi").expect("set tar path");
        header.set_size(8);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &b"echo hi\n"[..]).expect("append");
        builder.finish().expect("finish tar");
    }
    let blob = zstd::stream::encode_all(tar_bytes.as_slice(), 3).expect("zstd encode");

    let dst = TempDir::new().expect("tempdir");
    unpack_layer_into(&blob, dst.path()).expect("unpack zstd");
    let contents = fs::read(dst.path().join("usr/local/bin/hi")).expect("read");
    assert_eq!(contents, b"echo hi\n");
}

#[test]
fn applies_a_whiteout_marker() {
    let dst = TempDir::new().expect("tempdir");
    // Lower layer: create a file we'll later whiteout.
    fs::create_dir_all(dst.path().join("etc")).expect("mkdir");
    fs::write(dst.path().join("etc/foo"), b"lower").expect("write lower");

    // Upper layer: a whiteout marker for etc/foo.
    let blob = synth_layer_with("etc/.wh.foo", b"");
    unpack_layer_into(&blob, dst.path()).expect("unpack");

    assert!(
        !dst.path().join("etc/foo").exists(),
        "whiteout should remove etc/foo"
    );
}

#[test]
fn applies_an_opaque_marker() {
    let dst = TempDir::new().expect("tempdir");
    // Lower layer: a populated directory.
    fs::create_dir_all(dst.path().join("var/lib")).expect("mkdir");
    fs::write(dst.path().join("var/lib/old.txt"), b"lower").expect("write lower");

    // Upper layer: opaque marker + a new file in the same dir.
    // Two synth layers concatenated would be cleaner, but the unpacker
    // already handles whiteouts before regular entries so we can stage
    // them sequentially.
    let opaque = synth_layer_with("var/lib/.wh..wh..opq", b"");
    unpack_layer_into(&opaque, dst.path()).expect("unpack opaque");
    assert!(
        !dst.path().join("var/lib/old.txt").exists(),
        "opaque marker should clear lower entries"
    );

    let upper = synth_layer_with("var/lib/new.txt", b"upper");
    unpack_layer_into(&upper, dst.path()).expect("unpack upper");
    let contents = fs::read(dst.path().join("var/lib/new.txt")).expect("read upper");
    assert_eq!(contents, b"upper");
}

#[test]
fn refuses_path_traversal_entry() {
    // `dst` is a *subdir* of `outer` so a `../` escape would land in
    // `outer` where we can detect it without touching the real fs.
    let outer = TempDir::new().expect("tempdir");
    let dst = outer.path().join("rootfs");
    fs::create_dir_all(&dst).expect("mkdir rootfs");

    // A hostile layer entry trying to climb out of the rootfs.
    let blob = synth_layer_raw_name("../escape.txt", b"pwned");

    // Unpack succeeds — the unsafe entry is skipped, not fatal.
    unpack_layer_into(&blob, &dst).expect("unpack should skip the unsafe entry");

    assert!(
        !outer.path().join("escape.txt").exists(),
        "traversal entry escaped the rootfs into the parent dir"
    );
    assert!(
        !dst.join("escape.txt").exists(),
        "traversal entry should not be written inside the rootfs either"
    );
}

/// SECURITY: a whiteout whose path traverses a layer-planted symlink must
/// not delete files outside the rootfs (the delete must not follow the
/// symlink off-tree).
#[test]
fn whiteout_through_symlink_does_not_escape_rootfs() {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write as _;

    let sentinel = TempDir::new().expect("tempdir");
    fs::write(sentinel.path().join("victim"), b"host").expect("write");
    fs::write(sentinel.path().join("bystander"), b"host").expect("write");

    // Malicious layer: symlink `evil` -> <sentinel>, then whiteout + opaque
    // markers under `evil/` that would follow the symlink during the delete.
    let mut tar_bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(&mut h, "evil", sentinel.path())
            .expect("symlink entry");
        for marker in ["evil/.wh..wh..opq", "evil/.wh.victim"] {
            let mut m = tar::Header::new_gnu();
            m.set_entry_type(tar::EntryType::Regular);
            m.set_size(0);
            m.set_mode(0o644);
            m.set_cksum();
            b.append_data(&mut m, marker, std::io::empty())
                .expect("marker entry");
        }
        b.finish().expect("finish");
    }
    let mut blob = Vec::new();
    let mut enc = GzEncoder::new(&mut blob, Compression::default());
    enc.write_all(&tar_bytes).expect("gz");
    enc.finish().expect("gz finish");

    let dst = TempDir::new().expect("tempdir");
    let _ = unpack_layer_into(&blob, dst.path());

    assert!(
        sentinel.path().join("victim").exists(),
        "whiteout escaped: host file under a symlinked dir was deleted"
    );
    assert!(
        sentinel.path().join("bystander").exists(),
        "opaque escaped: host dir contents were cleared"
    );
}

#[test]
fn unsupported_media_type_is_rejected() {
    let layout_dir = TempDir::new().expect("tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init");

    // Forge a minimal manifest with an unsupported media-type.
    let cfg = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {},
        "rootfs": {"type": "layers", "diff_ids": ["sha256:deadbeef"]}
    });
    let cfg_bytes = serde_json::to_vec(&cfg).expect("json");
    let cfg_digest = layout.write_blob(&cfg_bytes).expect("write cfg");

    // Dummy layer blob.
    let layer_bytes = b"not really a tar".to_vec();
    let layer_digest = layout.write_blob(&layer_bytes).expect("write layer");

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": cfg_digest,
            "size": cfg_bytes.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": layer_digest,
            "size": layer_bytes.len(),
        }],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).expect("json");
    let manifest_digest = layout.write_blob(&manifest_bytes).expect("write manifest");
    let entry = oci_client::manifest::ImageIndexEntry {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        digest: manifest_digest,
        size: manifest_bytes.len() as i64,
        platform: None,
        annotations: None,
    };
    layout
        .upsert_ref("example.invalid/x:1", entry)
        .expect("upsert ref");

    let err = Bundle::from_image(&layout, "example.invalid/x:1", &BundleOptions::default())
        .expect_err("uncompressed layer should be rejected");
    match err {
        EngineError::UnsupportedLayerMediaType(mt) => {
            assert_eq!(mt, "application/vnd.oci.image.layer.v1.tar");
        }
        other => panic!("expected UnsupportedLayerMediaType, got {other:?}"),
    }
}

#[test]
fn from_scratch_stages_an_empty_bundle() {
    let bundle = Bundle::from_scratch(&BundleOptions::default()).expect("scratch bundle stages");

    // Empty rootfs, doubling as the single base lower — the same shape the
    // unpack path produces, so overlay stacking works unchanged.
    assert!(bundle.rootfs().is_dir());
    assert_eq!(
        std::fs::read_dir(bundle.rootfs()).unwrap().count(),
        0,
        "scratch rootfs starts empty",
    );
    assert_eq!(bundle.base_lowers().len(), 1);
    assert_eq!(bundle.base_lowers()[0], bundle.rootfs());

    // A runtime spec was written, same as for an image-backed bundle.
    assert!(bundle.path().join("config.json").is_file());
    assert!(bundle.image_entrypoint().is_empty());
    assert!(bundle.image_cmd().is_empty());
}
