//! Unit tests for the `disk` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::introspect::introspect;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;
use umf_parser::parse;

/// No prior container stages: the single-stage bootable path threads an empty
/// `produced` map into [`build_vm`]. A shared helper keeps every call site terse.
fn no_stages() -> BTreeMap<String, String> {
    BTreeMap::new()
}

/// Kernel-shaped tarball: `boot/vmlinuz-<release>` (content
/// `fake-kernel-image`) + a module under `lib/modules/<release>/`. The
/// bootable build's FROM kernel source.
fn synthetic_kernel_tarball(release: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        builder.mode(tar::HeaderMode::Deterministic);

        let vmlinuz = b"fake-kernel-image";
        let mut h = tar::Header::new_gnu();
        h.set_size(vmlinuz.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        builder
            .append_data(&mut h, format!("boot/vmlinuz-{release}"), &vmlinuz[..])
            .unwrap();

        let module = b"fake-module";
        let mut h = tar::Header::new_gnu();
        h.set_size(module.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        builder
            .append_data(
                &mut h,
                format!("lib/modules/{release}/kernel/fs/ext4.ko"),
                &module[..],
            )
            .unwrap();

        builder.finish().unwrap();
    }
    bytes
}

/// Minimal rootfs tarball — a couple of files so ROOTFS layering is
/// observable in the assembled staging tree.
fn synthetic_rootfs_tarball() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut bytes);
        b.mode(tar::HeaderMode::Deterministic);
        for (path, payload) in [
            ("etc/os-release", &b"NAME=\"Alpine Linux\"\n"[..]),
            ("bin/busybox", &b"#fake-busybox-ELF"[..]),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(payload.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, path, payload).unwrap();
        }
        b.finish().unwrap();
    }
    bytes
}

fn options_with_overrides(dir: &Path, release: &str) -> BootableBuildOptions {
    let kernel = dir.join("fake-kernel.tar");
    fs::write(&kernel, synthetic_kernel_tarball(release)).expect("seed kernel");
    BootableBuildOptions {
        from_kernel_path_override: Some(kernel),
        ..BootableBuildOptions::default()
    }
}

/// Source used by the "minimal" tests below. ENTRYPOINT is a binary path
/// (appliance), so initramfs generation is skipped (no rootfs to feed it).
const VM_SOURCE: &str = "FROM imagilux/kernel-linux:7.0\nLABEL org.imagilux.umf.flavor=systemd-boot\nENTRYPOINT /myapp\n";

#[tokio::test]
async fn single_stage_add_from_unknown_stage_errors() {
    // `ADD --from=<stage>` is wired now, but a single-stage recipe has no prior
    // stage to copy from — `produced` is empty, so the reference resolves to a
    // clear "no such prior stage" error rather than silently doing nothing.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let ast =
        parse("FROM imagilux/kernel-linux:7.0\nADD --from=builder /src /dst\n").expect("parse");
    let err = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:unknownstage",
        &options_with_overrides(dir.path(), "7.0"),
        &no_stages(),
    )
    .await
    .unwrap_err();
    match err {
        BootableBuildError::AddFromUnknownStage { stage } => assert_eq!(stage, "builder"),
        other => panic!("expected AddFromUnknownStage, got {other:?}"),
    }
}

#[tokio::test]
async fn local_add_lands_in_staging() {
    // A local `ADD <path> <dst>` copies from the build context onto the
    // rootfs, alongside the OCI userland and the FROM kernel. ENTRYPOINT is a
    // binary path (appliance), so no RUN / qemu is needed.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let release = "7.0";

    let context = dir.path().join("ctx");
    fs::create_dir_all(&context).expect("ctx");
    fs::write(context.join("extra.conf"), b"hello=world\n").expect("seed local file");

    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         ADD alpine:3.21.0 /\n\
         ADD ./extra.conf /etc/extra.conf\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("parse");

    let rootfs_tarball = dir.path().join("rootfs.tar");
    fs::write(&rootfs_tarball, synthetic_rootfs_tarball()).expect("seed rootfs");
    let kernel_tarball = dir.path().join("kernel.tar");
    fs::write(&kernel_tarball, synthetic_kernel_tarball(release)).expect("seed kernel");

    let staging_keep = dir.path().join("kept");
    let opts = BootableBuildOptions {
        rootfs_path_override: Some(rootfs_tarball),
        from_kernel_path_override: Some(kernel_tarball),
        staging_keep_path: Some(staging_keep),
        context: context.clone(),
        ..BootableBuildOptions::default()
    };

    let out = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:localadd",
        &opts,
        &no_stages(),
    )
    .await
    .expect("build_vm");

    let kept = out.staging_path.expect("staging persisted");
    // The local file landed on the rootfs…
    assert_eq!(
        fs::read(kept.join("etc/extra.conf")).expect("local file in rootfs"),
        b"hello=world\n"
    );
    // …alongside the OCI userland and the embedded kernel.
    assert!(kept.join("etc/os-release").is_file());
    assert!(kept.join(format!("boot/vmlinuz-{release}")).is_file());
}

#[tokio::test]
async fn local_add_rejects_parent_dir_traversal() {
    // A `..` in a local ADD source must be rejected before any join — it would
    // otherwise read host files outside the build context onto the rootfs.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         ADD alpine:3.21.0 /\n\
         ADD ../escape /loot\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("parse");

    let rootfs_tarball = dir.path().join("rootfs.tar");
    fs::write(&rootfs_tarball, synthetic_rootfs_tarball()).expect("seed rootfs");
    let opts = BootableBuildOptions {
        rootfs_path_override: Some(rootfs_tarball),
        ..options_with_overrides(dir.path(), "7.0")
    };
    let err = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:traversal",
        &opts,
        &no_stages(),
    )
    .await
    .unwrap_err();
    match err {
        BootableBuildError::AddPathTraversal { kind, .. } => assert_eq!(kind, "source"),
        other => panic!("expected AddPathTraversal, got {other:?}"),
    }
}

#[tokio::test]
async fn local_add_destination_substitutes_against_build_arg() {
    // `${VAR}` in a bootable ADD destination is substituted: an
    // in-stage `ARG` default, overridable by `--build-arg`. Appliance ENTRYPOINT
    // → no RUN / qemu. The bootable image keeps no per-directive history, so the
    // substituted value just drives placement, with nothing to leak.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");

    let context = dir.path().join("ctx");
    fs::create_dir_all(&context).expect("ctx");
    fs::write(context.join("greeting.txt"), b"hi\n").expect("seed local file");

    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         ARG DEST=/opt\n\
         ADD greeting.txt ${DEST}/\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("parse");

    let mut build_args = BTreeMap::new();
    build_args.insert("DEST".to_string(), "/srv".to_string());
    let staging_keep = dir.path().join("kept");
    let opts = BootableBuildOptions {
        context,
        build_args,
        staging_keep_path: Some(staging_keep),
        ..options_with_overrides(dir.path(), "7.0")
    };

    let out = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:argdst",
        &opts,
        &no_stages(),
    )
    .await
    .expect("build_vm");

    let kept = out.staging_path.expect("staging persisted");
    assert!(
        kept.join("srv/greeting.txt").is_file(),
        "file should land at the --build-arg destination /srv",
    );
    assert!(
        !kept.join("opt").exists(),
        "the declared default /opt must not be used once overridden",
    );
}

#[tokio::test]
async fn add_source_arg_expanding_to_traversal_is_rejected() {
    // The containment guard runs on the *substituted* source: an `ARG` whose
    // value carries `..` must not smuggle a traversal past it (security).
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         ARG EVIL=../../..\n\
         ADD ${EVIL}/etc/passwd /loot\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("parse");

    let opts = options_with_overrides(dir.path(), "7.0");
    let err = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:evil",
        &opts,
        &no_stages(),
    )
    .await
    .unwrap_err();
    match err {
        BootableBuildError::AddPathTraversal { kind, .. } => assert_eq!(kind, "source"),
        other => panic!("expected AddPathTraversal, got {other:?}"),
    }
}

#[tokio::test]
async fn builds_minimal_bootable_image() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let ast = parse(VM_SOURCE).expect("parse");
    let tag = "example.invalid/bootable:minimal";

    let staging_keep = dir.path().join("kept");
    let opts = BootableBuildOptions {
        staging_keep_path: Some(staging_keep),
        ..options_with_overrides(dir.path(), "7.0")
    };

    let out = build_vm(&ast, &layout, None, tag, &opts, &no_stages())
        .await
        .expect("build_vm");

    // The emitted artifact is a plain OCI image — there is no disk.
    assert!(!out.image.digest.is_empty());

    // It is `type=bootable` and carries the boot manifest.
    let profile = introspect(&layout, tag).expect("introspect");
    assert_eq!(profile.kind, L0Kind::Bootable);
    assert_eq!(
        profile.labels.get(label::ENTRYPOINT).map(String::as_str),
        Some("appliance"),
        "a binary ENTRYPOINT is the appliance shape"
    );
    assert_eq!(
        profile
            .labels
            .get(label::KERNEL_RELEASE)
            .map(String::as_str),
        Some("7.0")
    );
    assert_eq!(
        profile
            .labels
            .get(label::KERNEL_VMLINUZ)
            .map(String::as_str),
        Some("/boot/vmlinuz-7.0")
    );
    assert_eq!(
        profile.labels.get(label::FLAVOR).map(String::as_str),
        Some("systemd-boot")
    );

    // The embedded kernel landed in the image rootfs.
    let kept = out.staging_path.expect("staging persisted");
    assert_eq!(
        fs::read(kept.join("boot/vmlinuz-7.0")).expect("vmlinuz in rootfs"),
        b"fake-kernel-image"
    );
}

#[tokio::test]
async fn rootfs_and_from_kernel_layer_into_shared_staging() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let release = "7.0";
    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         ADD alpine:3.21.0 /\n\
         ENTRYPOINT systemd\n",
    )
    .expect("parse");

    let rootfs_tarball = dir.path().join("rootfs.tar");
    fs::write(&rootfs_tarball, synthetic_rootfs_tarball()).expect("seed rootfs");
    let kernel_tarball = dir.path().join("kernel.tar");
    fs::write(&kernel_tarball, synthetic_kernel_tarball(release)).expect("seed kernel");

    let staging_keep = dir.path().join("kept");
    let opts = BootableBuildOptions {
        rootfs_path_override: Some(rootfs_tarball.clone()),
        from_kernel_path_override: Some(kernel_tarball),
        staging_keep_path: Some(staging_keep),
        ..BootableBuildOptions::default()
    };

    let out = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:staging",
        &opts,
        &no_stages(),
    )
    .await
    .expect("build_vm");

    match out.add_sources.as_slice() {
        [crate::resolver::AddProvenance::Override(p)] => {
            assert_eq!(p, &rootfs_tarball)
        }
        other => panic!("expected one AddProvenance::Override, got {other:?}"),
    }
    match out.kernel_source {
        crate::resolver::FromKernelProvenance::Override(_) => {}
        other => panic!("expected FromKernelProvenance::Override, got {other:?}"),
    }
    assert_eq!(out.kernel_layout.release, release);

    // ADD --from userland (L1) + FROM kernel (L2) unioned into one staging tree — the
    // single layer the bootable image ships.
    let kept = out.staging_path.expect("staging persisted");
    assert!(kept.join("etc/os-release").is_file());
    assert!(kept.join(format!("boot/vmlinuz-{release}")).is_file());
    assert!(
        kept.join(format!("lib/modules/{release}/kernel/fs/ext4.ko"))
            .is_file(),
    );
}

#[tokio::test]
async fn initramfs_in_rootfs_when_entrypoint_is_init_system() {
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let release = "7.0";
    // ENTRYPOINT systemd → initramfs generation runs; the image ships it in
    // /boot and records its path in the boot manifest.
    let ast = parse(
        "FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         ADD alpine:3.21.0 /\n\
         ENTRYPOINT systemd\n",
    )
    .expect("parse");

    let rootfs_tarball = dir.path().join("rootfs.tar");
    fs::write(&rootfs_tarball, synthetic_rootfs_tarball()).expect("seed rootfs");

    // Kernel tarball with a squashfs module so the initramfs generator has
    // at least one module to bundle.
    let mut kernel_bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut kernel_bytes);
        b.mode(tar::HeaderMode::Deterministic);
        for (path, payload) in [
            (format!("boot/vmlinuz-{release}"), &b"fake-kernel"[..]),
            (
                format!("lib/modules/{release}/kernel/fs/squashfs/squashfs.ko"),
                &b"SQFS"[..],
            ),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(payload.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, &path, payload).unwrap();
        }
        b.finish().unwrap();
    }
    let kernel_tarball = dir.path().join("kernel.tar");
    fs::write(&kernel_tarball, &kernel_bytes).expect("seed kernel");

    let tag = "example.invalid/bootable:initramfs";
    let staging_keep = dir.path().join("kept");
    let opts = BootableBuildOptions {
        rootfs_path_override: Some(rootfs_tarball),
        from_kernel_path_override: Some(kernel_tarball),
        staging_keep_path: Some(staging_keep),
        ..BootableBuildOptions::default()
    };

    let out = build_vm(&ast, &layout, None, tag, &opts, &no_stages())
        .await
        .expect("build_vm");

    let report = out.initrd_report.expect("initrd_report present");
    assert_eq!(report.filename, format!("initramfs-{release}.img"));
    assert!(report.modules_count >= 1, "report: {report:?}");

    // The initramfs ships in the image rootfs at /boot/<filename>…
    let kept = out.staging_path.expect("staging persisted");
    let initrd = fs::read(kept.join("boot").join(&report.filename)).expect("initramfs in rootfs");
    assert_eq!(&initrd[..2], &[0x1f, 0x8b], "initramfs not gzip");

    // …and the boot manifest points at it.
    let profile = introspect(&layout, tag).expect("introspect");
    let want_initramfs = format!("/boot/{}", report.filename);
    assert_eq!(
        profile.labels.get(label::INITRAMFS).map(String::as_str),
        Some(want_initramfs.as_str())
    );
    assert_eq!(
        profile.labels.get(label::ENTRYPOINT).map(String::as_str),
        Some("systemd")
    );
}

// ── multi-stage bootable (cross-stage `ADD --from`) ─────────────────

/// Emit a minimal single-layer container image into `layout` under `ref_name`,
/// its rootfs carrying `file_path` → `contents`. Stands in for an earlier
/// container stage that a bootable `ADD --from=<stage>` reads from — the same
/// internal ref + `produced` mapping `build_container_stages` records.
fn emit_producer_image(layout: &ImageLayout, ref_name: &str, file_path: &str, contents: &[u8]) {
    let holder = tempdir().expect("producer tempdir");
    let root = holder.path().join("root");
    let abs = root.join(file_path);
    fs::create_dir_all(abs.parent().expect("file_path has a parent")).expect("producer parent");
    fs::write(&abs, contents).expect("producer file");
    let layer = LayerSource::from_directory_with(&root, LayerCompression::Gzip).expect("layer");
    let config = ImageConfig {
        architecture: Architecture::host().oci_arch_string().to_string(),
        os: "linux".to_string(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(layout, &[layer], &config, ref_name).expect("emit producer");
}

#[tokio::test]
async fn multi_stage_bootable_add_from_copies_into_rootfs() {
    // A prior container stage `prep` produces a file; the final bootable stage
    // copies it onto the rootfs with `ADD --from=prep`. This drives `build_vm`'s
    // cross-stage materialise + copy directly: the producer image is emitted and
    // recorded in `produced` exactly as the orchestrator's
    // `build_container_stages` would, without needing a real container build.
    let dir = tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("layout");
    let release = "7.0";

    emit_producer_image(
        &layout,
        "umf-build/stage-0:internal",
        "opt/payload.txt",
        b"from-prep\n",
    );
    let mut produced = BTreeMap::new();
    produced.insert("prep".to_string(), "umf-build/stage-0:internal".to_string());

    // Final stage is the bootable one (kernel FROM); its `FROM` decides the
    // shape. The earlier `FROM scratch AS prep` is a container producer.
    let ast = parse(
        "FROM scratch AS prep\n\
         FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         ADD --from=prep /opt/payload.txt /etc/from-prep.txt\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("parse");

    let kernel_tarball = dir.path().join("kernel.tar");
    fs::write(&kernel_tarball, synthetic_kernel_tarball(release)).expect("seed kernel");
    let staging_keep = dir.path().join("kept");
    let opts = BootableBuildOptions {
        from_kernel_path_override: Some(kernel_tarball),
        staging_keep_path: Some(staging_keep),
        ..BootableBuildOptions::default()
    };

    let out = build_vm(
        &ast,
        &layout,
        None,
        "example.invalid/bootable:crossstage",
        &opts,
        &produced,
    )
    .await
    .expect("build_vm");

    // The file copied out of the `prep` stage landed on the bootable rootfs,
    // alongside the embedded kernel.
    let kept = out.staging_path.expect("staging persisted");
    assert_eq!(
        fs::read(kept.join("etc/from-prep.txt")).expect("cross-stage file in rootfs"),
        b"from-prep\n"
    );
    assert!(kept.join(format!("boot/vmlinuz-{release}")).is_file());
}

#[test]
fn validate_ast_for_vm_picks_last_stage() {
    // The bootable stage is the FINAL stage. A multi-stage AST whose first stage
    // is `FROM scratch` (which would be rejected as a bootable FROM) and whose
    // last stage is a kernel `FROM` validates to the *last* stage — a guard on
    // the first→last shape decision.
    let ast = parse(
        "FROM scratch AS prep\n\
         FROM imagilux/kernel-linux:7.0\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("parse");
    let stage = validate_ast_for_vm(&ast).expect("last stage is the bootable stage");
    match &stage.from.source {
        FromSource::Reference(r) => assert_eq!(r.value.as_str(), "imagilux/kernel-linux:7.0"),
        FromSource::Scratch => {
            panic!("picked the scratch producer stage, not the final kernel stage")
        }
    }
}

#[test]
fn validate_ast_for_vm_rejects_container_only_directives() {
    // CMD / VOLUME / STOPSIGNAL are OCI container-config directives;
    // a bootable build must reject each at validation time.
    for (src, want) in [
        (
            "FROM imagilux/kernel-linux:7.0\nENTRYPOINT systemd\nCMD echo hi\n",
            "CMD",
        ),
        (
            "FROM imagilux/kernel-linux:7.0\nENTRYPOINT systemd\nVOLUME /data\n",
            "VOLUME",
        ),
        (
            "FROM imagilux/kernel-linux:7.0\nENTRYPOINT systemd\nSTOPSIGNAL SIGTERM\n",
            "STOPSIGNAL",
        ),
    ] {
        let ast = parse(src).expect("parse");
        match validate_ast_for_vm(&ast) {
            Err(BootableBuildError::ContainerOnlyDirective { directive }) => {
                assert_eq!(directive, want);
            }
            other => panic!("expected ContainerOnlyDirective({want}), got {other:?}"),
        }
    }
}

// ── appliance_init_cmdline (pure helper) ────────────────────────────
fn sp(s: &str) -> umf_core::ast::Spanned<String> {
    umf_core::ast::Spanned::new(s.to_string(), umf_core::ast::Span::new(0, 0))
}

#[test]
fn appliance_cmdline_path_form() {
    // Shell form is a single binary path — never whitespace-split into
    // argv (that would corrupt a path containing spaces, see below).
    assert_eq!(
        appliance_init_cmdline(&EntrypointInit::Path(sp("/myapp"))),
        Some(" init=/myapp".to_string())
    );
}

#[test]
fn appliance_cmdline_path_form_preserves_spaces_in_binary_path() {
    // Regression: a shell-form binary path containing spaces must
    // stay one token — quoted so the kernel cmdline tokenizer keeps it
    // intact — not split into `init=/opt/my -- app/run`.
    assert_eq!(
        appliance_init_cmdline(&EntrypointInit::Path(sp("/opt/my app/run"))),
        Some(" init=\"/opt/my app/run\"".to_string())
    );
}

#[test]
fn appliance_cmdline_exec_form_quotes_whitespace_args() {
    let argv = vec![sp("/usr/sbin/nginx"), sp("-g"), sp("daemon off;")];
    assert_eq!(
        appliance_init_cmdline(&EntrypointInit::Exec(argv)),
        Some(" init=/usr/sbin/nginx -- -g \"daemon off;\"".to_string())
    );
}

#[test]
fn appliance_cmdline_none_for_init_systems_and_none() {
    assert_eq!(appliance_init_cmdline(&EntrypointInit::Systemd), None);
    assert_eq!(appliance_init_cmdline(&EntrypointInit::OpenRc), None);
    assert_eq!(appliance_init_cmdline(&EntrypointInit::None), None);
}
