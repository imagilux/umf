//! End-to-end CLI integration tests for the `umf` binary.
//!
//! These spawn the actual built binary via `assert_cmd`, exercise the
//! `parse` subcommand against fixture `.umf` files under `tests/fixtures/`,
//! and snapshot the JSON AST output via `insta`.
//!
//! **Updating snapshots** — when an intentional output change lands, run
//! `cargo insta review` (or `INSTA_UPDATE=always cargo test --test cli`)
//! to accept the new snapshots; review the resulting `.snap.new` files
//! before committing.
//!
//! Snapshots live in `tests/snapshots/`. JSON is filtered to mask source
//! `span` byte offsets so cosmetic source rewrites don't churn snapshots.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;

fn umf() -> Command {
    let mut cmd = Command::cargo_bin("umf").expect("umf binary should exist in cargo's target dir");
    // Suppress ANSI escape codes so substring assertions match raw text.
    cmd.env("NO_COLOR", "1");
    cmd
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

// Fixture files under `tests/fixtures/` are immutable test data — byte
// offsets in the resulting AST stay stable as long as no one edits them, so
// snapshots don't need filtering. If a fixture is intentionally updated, run
// `cargo insta review` to accept the new offsets.

#[test]
fn help_prints_subcommands() {
    umf()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("parse"))
        .stdout(contains("build"))
        .stdout(contains("run"))
        .stdout(contains("inspect"))
        .stdout(contains("doctor"));
}

#[test]
fn help_lists_global_trace_flags() {
    umf()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("--trace-format"))
        .stdout(contains("--trace-output"))
        .stdout(contains("--trace-level"));
}

#[test]
fn trace_format_json_emits_structured_spans_on_stderr() {
    // Run any subcommand that exercises an instrumented phase. `parse`
    // is the simplest — it hits `umf.parse` via `umf_parser::parse`.
    let out = umf()
        .args([
            "--trace-format=json",
            "parse",
            fixture("minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let text = String::from_utf8(out).expect("stderr utf8");

    // Each non-empty stderr line must be valid JSON.
    let mut saw_umf_parse = false;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("non-JSON line in --trace-format=json output: {line:?} — {e}")
        });
        if parsed
            .get("span")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            == Some("umf.parse")
        {
            saw_umf_parse = true;
        }
    }
    assert!(
        saw_umf_parse,
        "expected at least one span named `umf.parse` in stderr — got: {text}",
    );
}

#[test]
fn trace_format_text_is_default_unchanged() {
    // Without the flag, output shape stays text. `umf parse` should not
    // litter stderr with structured span JSON — i.e. no `{"timestamp":`
    // prefix on any stderr line.
    let out = umf()
        .args(["parse", fixture("minimal.umf").to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let text = String::from_utf8(out).expect("stderr utf8");
    for line in text.lines() {
        assert!(
            !line.starts_with(r#"{"timestamp""#),
            "default format leaked JSON span event: {line:?}",
        );
    }
}

#[test]
fn run_help_lists_container_flags() {
    umf()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(contains("--interactive"))
        .stdout(contains("--env"))
        .stdout(contains("--entrypoint"))
        .stdout(contains("--keep-bundle"));
}

#[test]
fn run_help_lists_vm_flags() {
    umf()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(contains("--vmm"))
        .stdout(contains("--disk"))
        .stdout(contains("--firmware"))
        .stdout(contains("--memory"))
        .stdout(contains("--cpus"))
        .stdout(contains("--port-forward"))
        .stdout(contains("--graphic"));
}

#[test]
fn run_vmm_on_container_without_disk_errors_helpfully() {
    // `--vmm` on a container (no `--disk`, not a bootable image) is a user
    // error — a container can't boot in a VM. A bootable image, by contrast,
    // auto-compiles and boots without `--disk`.
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/fake-container:vmm";
    let cfg = ImageConfig {
        umf_type: L0Kind::Container,
        container: ContainerConfig::default(),
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit fake container");

    umf()
        .args(["run", "--vmm=qemu", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(ref_name)
        .assert()
        .failure()
        .stderr(contains("bootable-OS image"));
}

#[test]
fn run_vm_flag_on_container_errors_helpfully() {
    // A VM-boot flag (`--memory`) against a container image is silently
    // dropped today — assert it's now rejected with a pointer to the right
    // shape. No network: the container is staged directly in a temp layout.
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/fake-container:vmflag";
    let cfg = ImageConfig {
        umf_type: L0Kind::Container,
        container: ContainerConfig::default(),
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit fake container");

    umf()
        .args(["run", "--memory=512", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(ref_name)
        .assert()
        .failure()
        .stderr(contains("--memory is a VM-boot flag"));
}

#[test]
fn run_disk_without_vmm_errors_helpfully() {
    umf()
        .args(["run", "--disk", "/tmp/x.raw", "fake-ref"])
        .assert()
        .failure()
        .stderr(contains("--disk only meaningful with --vmm"));
}

#[test]
fn run_unknown_vmm_backend_errors_with_hint() {
    umf()
        .args([
            "run",
            "--vmm=bochs",
            "--disk",
            "/tmp/nonexistent.raw",
            "fake-ref",
        ])
        .assert()
        .failure()
        .stderr(contains("unknown --vmm backend"))
        .stderr(contains("qemu"))
        .stderr(contains("ch"));
}

#[test]
fn run_help_mentions_both_vmm_backends() {
    umf()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(contains("qemu"))
        .stdout(contains("ch"))
        .stdout(contains("Cloud Hypervisor"));
}

#[test]
fn images_on_empty_layout_prints_friendly_message() {
    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    umf()
        .args(["images", "--layout-dir"])
        .arg(layout_dir.path())
        .assert()
        .success()
        .stdout(contains("(no images in layout)"));
}

#[test]
fn images_table_lists_staged_ref() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/images-test:latest";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");

    umf()
        .args(["images", "--layout-dir"])
        .arg(layout_dir.path())
        .assert()
        .success()
        .stdout(contains("REFERENCE"))
        .stdout(contains(ref_name))
        .stdout(contains("container"));
}

#[test]
fn images_json_format_round_trips() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/images-json:latest";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");

    let out = umf()
        .args(["images", "--format=json", "--layout-dir"])
        .arg(layout_dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("images --format=json should produce valid JSON");
    let arr = parsed.as_array().expect("array root");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["reference"].as_str(), Some(ref_name));
    assert_eq!(arr[0]["umf_type"].as_str(), Some("container"));
    assert!(arr[0]["digest"].as_str().unwrap().starts_with("sha256:"));
}

#[test]
fn images_remove_drops_ref_from_layout() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/rm-test:latest";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");

    umf()
        .args(["images", "--layout-dir"])
        .arg(layout_dir.path())
        .args(["--remove", ref_name])
        .assert()
        .success()
        .stdout(contains("Untagged"))
        .stdout(contains(ref_name));

    // After the remove, the ref is gone from the layout.
    assert!(layout.lookup_ref(ref_name).expect("lookup").is_none());
}

#[test]
fn images_remove_accepts_multiple_refs() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let refs = [
        "example.invalid/multi-rm-a:latest",
        "example.invalid/multi-rm-b:latest",
        "example.invalid/multi-rm-c:latest",
    ];
    for r in &refs {
        let cfg = ImageConfig {
            container: ContainerConfig::default(),
            umf_type: L0Kind::Container,
            ..ImageConfig::default()
        };
        emit_image(&layout, &[], &cfg, r).expect("emit");
    }

    umf()
        .args(["images", "--layout-dir"])
        .arg(layout_dir.path())
        .arg("--remove")
        .args(refs)
        .assert()
        .success()
        .stdout(contains(refs[0]))
        .stdout(contains(refs[1]))
        .stdout(contains(refs[2]));

    for r in &refs {
        assert!(
            layout.lookup_ref(r).expect("lookup").is_none(),
            "{r} should be gone after remove",
        );
    }
}

#[test]
fn images_remove_with_prune_gcs_unreachable_blobs() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/prune-test:latest";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");

    let blobs_dir = layout_dir.path().join("blobs").join("sha256");
    let before = std::fs::read_dir(&blobs_dir).expect("read blobs").count();
    assert!(before > 0, "should have blobs before prune");

    umf()
        .args(["images", "--prune", "--layout-dir"])
        .arg(layout_dir.path())
        .args(["--remove", ref_name])
        .assert()
        .success()
        .stdout(contains("Pruned"));

    let after = std::fs::read_dir(&blobs_dir)
        .expect("read blobs after")
        .count();
    assert!(
        after < before,
        "prune should have removed blobs: before={before}, after={after}",
    );
}

#[test]
fn images_prune_only_runs_without_remove() {
    // `--prune` without `--remove` is the "GC orphans" use case —
    // stage an image, drop its ref via the layout API directly (no
    // CLI), then verify `images --prune` cleans up the orphaned
    // blobs.
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");
    let ref_name = "example.invalid/orphan:latest";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");
    assert!(layout.remove_ref(ref_name).expect("remove"));

    let blobs_dir = layout_dir.path().join("blobs").join("sha256");
    let before = std::fs::read_dir(&blobs_dir).expect("read blobs").count();
    assert!(before > 0);

    umf()
        .args(["images", "--prune", "--layout-dir"])
        .arg(layout_dir.path())
        .assert()
        .success()
        .stdout(contains("Pruned"));

    let after = std::fs::read_dir(&blobs_dir)
        .expect("read blobs after")
        .count();
    assert!(after < before);
}

#[test]
fn push_subcommand_errors_when_ref_not_in_layout() {
    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    umf()
        .args(["push", "--layout-dir"])
        .arg(layout_dir.path())
        .arg("example.invalid/nope:latest")
        .assert()
        .failure()
        .stderr(contains("ref not found in layout"));
}

#[test]
fn help_lists_layout_management_subcommands() {
    // `rmi` no longer exists — removal is `umf images --remove`.
    // Push/pull stay as standalone top-level subcommands (they're
    // network actions, not local-layout management).
    let assert = umf().arg("--help").assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8");
    for subcommand in ["images", "push", "pull"] {
        assert!(
            out.contains(subcommand),
            "expected `{subcommand}` in help; got: {out}",
        );
    }
    assert!(
        !out.contains(" rmi"),
        "rmi subcommand should be gone (subsumed by `images --remove`); got: {out}",
    );
}

#[test]
fn images_help_lists_action_flags() {
    umf()
        .args(["images", "--help"])
        .assert()
        .success()
        .stdout(contains("--list"))
        .stdout(contains("--remove"))
        .stdout(contains("--prune"))
        .stdout(contains("--format"));
}

#[test]
fn save_then_load_round_trips_through_cli() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let src_dir = tempfile::tempdir().expect("src layout");
    let src = ImageLayout::init(src_dir.path()).expect("init src");
    let ref_name = "example.invalid/archive-rt:1";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&src, &[], &cfg, ref_name).expect("emit");

    let tar_file = tempfile::NamedTempFile::new().expect("tarball");
    umf()
        .args(["save", "--layout-dir"])
        .arg(src_dir.path())
        .arg("--output")
        .arg(tar_file.path())
        .arg(ref_name)
        .assert()
        .success()
        .stderr(contains("Saved 1 ref(s)"));

    let dst_dir = tempfile::tempdir().expect("dst layout");
    umf()
        .args(["load", "--layout-dir"])
        .arg(dst_dir.path())
        .arg("--input")
        .arg(tar_file.path())
        .assert()
        .success()
        .stderr(contains("Loaded 1 ref(s)"))
        .stderr(contains(ref_name));

    // Confirm the loaded layout sees the ref with the same digest.
    let dst = ImageLayout::init(dst_dir.path()).expect("reopen dst");
    let after = dst.lookup_ref(ref_name).expect("lookup").expect("present");
    let before = src.lookup_ref(ref_name).expect("lookup").expect("present");
    assert_eq!(before.digest, after.digest);
}

#[test]
fn save_help_lists_expected_flags() {
    umf()
        .args(["save", "--help"])
        .assert()
        .success()
        .stdout(contains("--output"))
        .stdout(contains("--layout-dir"));
}

#[test]
fn load_help_lists_expected_flags() {
    umf()
        .args(["load", "--help"])
        .assert()
        .success()
        .stdout(contains("--input"))
        .stdout(contains("--overwrite"))
        .stdout(contains("--layout-dir"));
}

#[test]
fn debug_help_mentions_build_target() {
    umf()
        .args(["debug", "--help"])
        .assert()
        .success()
        .stdout(contains("build"));
}

#[test]
fn debug_build_help_lists_expected_flags() {
    umf()
        .args(["debug", "build", "--help"])
        .assert()
        .success()
        .stdout(contains("--tag"))
        .stdout(contains("--layout-dir"))
        .stdout(contains("--break-on"));
}

#[test]
fn help_lists_debug_subcommand() {
    umf()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("debug"));
}

#[test]
fn bench_help_lists_expected_flags() {
    umf()
        .args(["bench", "--help"])
        .assert()
        .success()
        .stdout(contains("--runs"))
        .stdout(contains("--warmup"))
        .stdout(contains("--cold-only"))
        .stdout(contains("--format"))
        .stdout(contains("--layout-dir"))
        .stdout(contains("--tag"));
}

#[test]
fn help_lists_bench_subcommand() {
    umf()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("bench"));
}

#[test]
fn build_help_lists_metrics_flags() {
    umf()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(contains("--metrics"))
        .stdout(contains("--metrics-output"));
}

/// `umf inspect <ref>` for a staged container image surfaces every
/// piece of the report (target type, entrypoint, env, labels, layer
/// count). No network — uses the umf-oci primitives to stage a real
/// emitted image in a temp layout.
#[test]
fn inspect_renders_table_with_expected_fields() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let ref_name = "example.invalid/inspect-test:latest";
    let cfg = ImageConfig {
        container: ContainerConfig {
            entrypoint: Some(vec!["/usr/local/bin/hello".to_string()]),
            cmd: Some(vec!["--foo".to_string()]),
            env: vec![
                "PATH=/usr/local/bin:/usr/bin".to_string(),
                "FOO=bar".to_string(),
            ],
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");

    umf()
        .args(["inspect", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(ref_name)
        .assert()
        .success()
        .stdout(contains("Reference:"))
        .stdout(contains("Target type:"))
        .stdout(contains("Container"))
        .stdout(contains("ENTRYPOINT  /usr/local/bin/hello"))
        .stdout(contains("CMD         --foo"))
        .stdout(contains("FOO=bar"))
        .stdout(contains("Labels"))
        .stdout(contains("org.imagilux.umf.type"));
}

#[test]
fn inspect_json_format_produces_parseable_output() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let ref_name = "example.invalid/inspect-json:latest";
    let cfg = ImageConfig {
        container: ContainerConfig {
            entrypoint: Some(vec!["/bin/sh".to_string()]),
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit");

    let output = umf()
        .args(["inspect", "--format=json", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(ref_name)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("stdout utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("inspect --format=json should produce valid JSON");
    assert_eq!(parsed["reference"].as_str(), Some(ref_name));
    assert!(parsed["target"]["kind"].is_string());
    assert_eq!(parsed["runtime"]["entrypoint"][0].as_str(), Some("/bin/sh"));
    assert!(parsed["labels"].is_object());
    assert!(
        parsed["manifest"]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

/// `umf run` against a bootable image auto-compiles it (the build→compile→run
/// fusion) instead of rejecting the target. This fake image carries `type=
/// bootable` but no boot manifest, so the auto-compile fails at the missing
/// kernel label — which still proves `run` dispatched into the projector rather
/// than refusing. No network: the image is staged directly in a temp layout.
#[test]
fn run_bootable_image_auto_compiles() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let layout = ImageLayout::init(layout_dir.path()).expect("init layout");

    let ref_name = "example.invalid/fake-bootable:dispatch-test";
    let cfg = ImageConfig {
        container: ContainerConfig::default(),
        umf_type: L0Kind::Bootable,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, ref_name).expect("emit fake bootable");

    umf()
        .args(["run", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(ref_name)
        .assert()
        .failure()
        .stderr(contains("compile"))
        .stderr(contains("boot-manifest label"));
}

/// Full CLI end-to-end: `umf build` a tiny recipe, then `umf run` the
/// resulting tag and assert exit propagation. The acceptance criterion
/// in one test.
///
/// Gated on `UMF_ENGINE_SMOKE=1` — same gate as the build smoke tests
/// (needs network for the base-image pull + `CAP_SYS_ADMIN` for the
/// overlay mount the build engine uses).
#[test]
fn build_then_run_round_trips_exit_code() {
    if std::env::var("UMF_ENGINE_SMOKE")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        eprintln!("skipping: set UMF_ENGINE_SMOKE=1 to run this test");
        return;
    }

    let ctx = tempfile::tempdir().expect("ctx tempdir");
    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    let recipe = ctx.path().join("smoke.umf");
    std::fs::write(
        &recipe,
        "FROM alpine:3.21\nENTRYPOINT [\"/bin/sh\", \"-c\", \"exit 17\"]\n",
    )
    .expect("write recipe");

    let tag = "example.invalid/run-smoke:latest";

    // Build.
    umf()
        .args(["build", "--tag", tag, "--layout-dir"])
        .arg(layout_dir.path())
        .arg(&recipe)
        .assert()
        .success();

    // Run. Exit 17 must propagate as the CLI's exit code.
    umf()
        .args(["run", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(tag)
        .assert()
        .code(17);
}

#[test]
fn doctor_reports_host_state() {
    umf()
        .arg("doctor")
        .assert()
        .success()
        // Sectioned table: Container + VM sections, column headers, key rows.
        .stdout(contains("Container build & RUN"))
        .stdout(contains("VM / bootable"))
        .stdout(contains("STATUS"))
        .stdout(contains("container runtime"))
        .stdout(contains("qemu-system-x86_64"))
        .stdout(contains("/dev/kvm"));
}

#[test]
fn doctor_with_file_scopes_to_that_build() {
    // The minimal VM fixture is an appliance (no RUN), and doctor's cache-only
    // probe can't confirm bootable-ness offline — either way it needs nothing
    // on the host, so the verdict is "ready to build".
    umf()
        .args(["doctor", fixture("vm_minimal.umf").to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("This build needs:"))
        .stdout(contains("Status: ready to build."));
}

#[test]
fn parse_minimal_succeeds() {
    // Default format is the table summary: stage heading, FROM row, etc.
    umf()
        .args(["parse", fixture("minimal.umf").to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Stage 1"))
        .stdout(contains("FROM"))
        .stdout(contains("scratch"));
}

#[test]
fn parse_unknown_directive_fails() {
    // ariadne colors each highlighted character individually so the source
    // span chars ("FOOBAR") aren't a contiguous substring — match the
    // diagnostic title (uncolored) instead.
    umf()
        .args([
            "parse",
            fixture("invalid_unknown_directive.umf").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("expected a directive keyword"));
}

#[test]
fn parse_format_json_emits_valid_json() {
    let output = umf()
        .args([
            "parse",
            "--format=json",
            fixture("minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("stdout should be UTF-8");
    // Round-trip through serde_json to confirm well-formedness.
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("output should be valid JSON");
    assert!(parsed["stages"].is_array(), "expected `stages` array");
}

#[test]
fn parse_minimal_json_snapshot() {
    let output = umf()
        .args([
            "parse",
            "--format=json",
            fixture("minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("stdout should be UTF-8");
    insta::assert_snapshot!("minimal_ast", text);
}

#[test]
fn parse_container_json_snapshot() {
    let output = umf()
        .args([
            "parse",
            "--format=json",
            fixture("container.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("stdout should be UTF-8");
    insta::assert_snapshot!("container_ast", text);
}

#[test]
fn parse_vm_json_snapshot() {
    let output = umf()
        .args([
            "parse",
            "--format=json",
            fixture("vm.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("stdout should be UTF-8");
    insta::assert_snapshot!("vm_ast", text);
}

#[test]
fn build_requires_tag() {
    umf()
        .args(["build", fixture("container.umf").to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("--tag"));
}

#[test]
fn build_staging_keep_on_container_is_rejected() {
    // `--staging-keep` only persists a bootable build's staging tree; on a
    // container build it was silently ignored. It must now error before any
    // build work. `FROM scratch` routes to the container path offline.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Containerfile"), "FROM scratch\n").expect("write recipe");
    let keep = dir.path().join("staging");
    umf()
        .args(["build", "--tag", "example.invalid/c:1", "--staging-keep"])
        .arg(&keep)
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(contains(
            "--staging-keep is only meaningful for bootable builds",
        ));
}

#[test]
fn build_metrics_output_on_bootable_is_rejected() {
    // The bootable path emits no metrics report, so `--metrics-output` wrote
    // nothing — it must now error. A `FROM` kernel routes to the bootable
    // pipeline; seed the kernel so the FROM probe resolves it offline.
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    seed_kernel_image(&layout_dir, "imagilux/kernel-linux:7.0", "7.0");
    let metrics = scratch.path().join("metrics.json");
    umf()
        .args([
            "build",
            "--tag",
            "example.invalid/bootable:metrics",
            "--metrics-output",
            metrics.to_str().unwrap(),
            "--layout-dir",
            layout_dir.to_str().unwrap(),
            fixture("vm_minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains(
            "--metrics-output is only meaningful for container builds",
        ));
}

#[test]
fn build_discovers_containerfile_in_directory() {
    // Pointing `build` at a directory discovers `Containerfile`. Reaching
    // the `--tag` error (rather than a read error) proves it found and
    // parsed the recipe by name, not by extension.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Containerfile"), "FROM scratch\n").expect("write recipe");
    umf()
        .arg("build")
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(contains("--tag"));
}

#[test]
fn build_falls_back_to_dockerfile_in_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").expect("write recipe");
    umf()
        .arg("build")
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(contains("--tag"));
}

#[test]
fn build_with_no_path_discovers_in_cwd() {
    // Bare `umf build` defaults the positional to the current directory.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Containerfile"), "FROM scratch\n").expect("write recipe");
    umf()
        .arg("build")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("--tag"));
}

#[test]
fn build_empty_directory_errors_with_discovery_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    umf()
        .arg("build")
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(contains("no recipe found"))
        .stderr(contains("Containerfile"))
        .stderr(contains("Dockerfile"))
        .stderr(contains("-f/--file"));
}

#[test]
fn build_file_override_points_at_any_name() {
    // `-f` selects a recipe of any name; the positional is the context.
    let dir = tempfile::tempdir().expect("tempdir");
    let recipe = dir.path().join("prod.recipe");
    std::fs::write(&recipe, "FROM scratch\n").expect("write recipe");
    umf()
        .arg("build")
        .arg("-f")
        .arg(&recipe)
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(contains("--tag"));
}

#[test]
fn build_help_lists_file_flag() {
    umf()
        .args(["build", "--help"])
        .assert()
        .success()
        .stdout(contains("--file"));
}

#[test]
fn parse_discovers_recipe_in_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Containerfile"), "FROM scratch\n").expect("write recipe");
    umf()
        .arg("parse")
        .arg(dir.path())
        .assert()
        .success()
        .stdout(contains("Containerfile"))
        .stdout(contains("FROM"))
        .stdout(contains("scratch"));
}

#[test]
fn parse_file_override_accepts_extensionless_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recipe = dir.path().join("recipe");
    std::fs::write(&recipe, "FROM scratch\n").expect("write recipe");
    umf()
        .arg("parse")
        .arg("-f")
        .arg(&recipe)
        .assert()
        .success()
        .stdout(contains("FROM"))
        .stdout(contains("scratch"));
}

#[test]
fn build_bootable_requires_tag() {
    // A `FROM` kernel routes to the bootable pipeline, which — like a container
    // build — emits an OCI image and so requires --tag. The kernel image is
    // seeded into the layout so the FROM probe resolves it (bootable) offline.
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    seed_kernel_image(&layout_dir, "imagilux/kernel-linux:7.0", "7.0");
    umf()
        .args([
            "build",
            "--layout-dir",
            layout_dir.to_str().unwrap(),
            fixture("vm_minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("--tag is required for bootable"));
}

/// Emit a synthetic `type=kernel` OCI image (`boot/vmlinuz-<release>` plus one
/// module) into `layout_dir` under `reference`, so an offline `umf build` can
/// `FROM` it (resolved from the layout cache — the OCI-native replacement for
/// the old `--from-kernel-path` seam).
fn seed_kernel_image(layout_dir: &std::path::Path, reference: &str, release: &str) {
    seed_image(
        layout_dir,
        reference,
        umf_core::l0::L0Kind::Payload(umf_core::l0::Payload::Kernel),
        &[
            (format!("boot/vmlinuz-{release}"), b"fake-kernel".to_vec()),
            (
                format!("lib/modules/{release}/kernel/fs/ext4.ko"),
                b"fake-mod".to_vec(),
            ),
        ],
    );
}

/// Emit a synthetic `type=rootfs` OCI image carrying `files` (path, bytes) into
/// `layout_dir` under `reference`, so `ADD <reference> /` pulls it offline.
fn seed_rootfs_image(layout_dir: &std::path::Path, reference: &str, files: &[(String, Vec<u8>)]) {
    seed_image(
        layout_dir,
        reference,
        umf_core::l0::L0Kind::Payload(umf_core::l0::Payload::Rootfs),
        files,
    );
}

/// Emit an OCI image of `kind` carrying `files` into the layout under `reference`.
fn seed_image(
    layout_dir: &std::path::Path,
    reference: &str,
    kind: umf_core::l0::L0Kind,
    files: &[(String, Vec<u8>)],
) {
    use umf_core::architecture::Architecture;
    use umf_oci::image::{ContainerConfig, ImageConfig, LayerSource, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout = ImageLayout::init(layout_dir).expect("init layout");
    let dir = tempfile::tempdir().expect("image dir");
    for (path, payload) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, payload).unwrap();
    }
    let layer = LayerSource::from_directory(dir.path()).expect("layer");
    let cfg = ImageConfig {
        architecture: Architecture::host().oci_arch_string().to_string(),
        os: "linux".to_string(),
        umf_type: kind,
        container: ContainerConfig::default(),
        ..ImageConfig::default()
    };
    // Store under the canonical ref (`Reference::whole()`, e.g.
    // `docker.io/imagilux/kernel-linux:7.0`) — the key the resolver and the
    // FROM probe look up, mirroring what a real registry pull would cache.
    let canonical = reference
        .parse::<oci_client::Reference>()
        .map_or_else(|_| reference.to_string(), |r| r.whole());
    emit_image(&layout, &[layer], &cfg, &canonical).expect("emit image");
}

#[test]
fn build_bootable_with_kernel_and_rootfs_layers_staging() {
    // FROM is a kernel image and the rootfs is `ADD <oci-ref> /`. Both are
    // seeded into the layout so the build resolves them offline (no flags), and
    // we check the staging tree carries rootfs + kernel content layered
    // correctly. `build` emits an OCI image (no disk).
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    let release = "6.6.79";
    seed_kernel_image(&layout_dir, "imagilux/kernel-linux:6.6.79", release);
    seed_rootfs_image(
        &layout_dir,
        "alpine:3.21.0",
        &[
            (
                "etc/os-release".to_string(),
                b"NAME=\"Alpine Linux\"\n".to_vec(),
            ),
            ("bin/busybox".to_string(), b"#fake-busybox-ELF".to_vec()),
        ],
    );
    let staging_keep = scratch.path().join("staging");

    umf()
        .args([
            "build",
            "--tag",
            "example.invalid/bootable:from-kernel",
            "--staging-keep",
            staging_keep.to_str().unwrap(),
            "--layout-dir",
            layout_dir.to_str().unwrap(),
            fixture("vm_with_kernel.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains(format!("release {release}")));

    assert!(staging_keep.join("etc/os-release").is_file());
    assert!(
        staging_keep
            .join(format!("boot/vmlinuz-{release}"))
            .is_file(),
        "missing vmlinuz in staging",
    );
    assert!(
        staging_keep
            .join(format!("lib/modules/{release}/kernel/fs/ext4.ko"))
            .is_file(),
        "missing kernel module in staging",
    );
}

#[test]
fn build_bootable_produces_an_image() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    seed_kernel_image(&layout_dir, "imagilux/kernel-linux:7.0", "7.0");

    umf()
        .args([
            "build",
            "--tag",
            "example.invalid/bootable:minimal",
            "--layout-dir",
            layout_dir.to_str().unwrap(),
            fixture("vm_minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("Built bootable image"))
        .stdout(contains("type: bootable"))
        .stdout(contains("flavor: systemd-boot"));

    // The artifact is a plain layered OCI image in the cache layout — there is
    // no disk (a disk is a `umf compile` projection).
    assert!(
        layout_dir.join("index.json").is_file(),
        "bootable image not written to the OCI layout",
    );
}

// End-to-end container builds via `umf build` are covered by
// `crates/umf-builder/tests/engine_build_smoke.rs` (gated on
// `UMF_ENGINE_SMOKE=1` because the umf-engine path needs network +
// CAP_SYS_ADMIN for the overlay mount).

#[test]
fn compile_projects_a_bootable_image_to_a_disk() {
    // Full build/compile loop: `umf build` a bootable image to OCI, then
    // `umf compile` it into a disk. The bootloader comes from the rootfs image
    // (in-image `/usr/lib/systemd/boot/efi/`), so the projection runs offline
    // with no host bootloader and no flag.
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    let tag = "example.invalid/bootable:compile";
    build_compilable_bootable(scratch.path(), &layout_dir, tag);

    let disk = scratch.path().join("disk.img");
    umf()
        .args([
            "compile",
            tag,
            "-o",
            disk.to_str().unwrap(),
            "--disk-size",
            "268435456",
            "--esp-size",
            "67108864",
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("Compiled"))
        .stdout(contains("flavor: systemd-boot"));

    let bytes = std::fs::read(&disk).expect("read disk");
    assert_eq!(&bytes[510..512], &[0x55, 0xAA], "protective MBR missing");
    assert_eq!(&bytes[512..520], b"EFI PART", "GPT signature missing");
}

#[test]
fn compile_missing_image_reports_not_in_layout() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    umf()
        .args([
            "compile",
            "example.invalid/absent:1",
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("not in the local layout"));
}

/// Seed a kernel image and an `imagilux/rootfs:1.0` image carrying an in-image
/// systemd-boot `.efi`, then build a FROM-kernel + `ADD`-rootfs appliance recipe
/// into `layout_dir` at `tag`. The result compiles offline: the classic flavor
/// finds the bootloader the rootfs ships (no host bootloader, no flag).
fn build_compilable_bootable(scratch: &std::path::Path, layout_dir: &std::path::Path, tag: &str) {
    use umf_core::architecture::Architecture;
    seed_kernel_image(layout_dir, "imagilux/kernel-linux:7.0", "7.0");
    let mut efi = vec![b'M', b'Z', 0x90, 0x00];
    efi.extend_from_slice(&[0u8; 252]);
    let efi_path = format!(
        "usr/lib/systemd/boot/efi/{}",
        Architecture::host().systemd_boot_filename()
    );
    seed_rootfs_image(layout_dir, "imagilux/rootfs:1.0", &[(efi_path, efi)]);
    let recipe = scratch.join("compilable.umf");
    std::fs::write(
        &recipe,
        "FROM imagilux/kernel-linux:7.0\n\
         LABEL org.imagilux.umf.flavor=systemd-boot\n\
         ADD imagilux/rootfs:1.0 /\n\
         ENTRYPOINT /myapp\n",
    )
    .expect("write recipe");
    umf()
        .args([
            "build",
            "--tag",
            tag,
            "--layout-dir",
            layout_dir.to_str().unwrap(),
            recipe.to_str().unwrap(),
        ])
        .assert()
        .success();
}

/// Build a bootable image into `layout_dir` and compile it into the block cache
/// (default geometry, so `save --type=block` finds it). Returns the tag.
fn build_and_compile_bootable(scratch: &std::path::Path, layout_dir: &std::path::Path) -> String {
    let tag = "example.invalid/bootable:save".to_string();
    build_compilable_bootable(scratch, layout_dir, &tag);
    umf()
        .args([
            "compile",
            &tag,
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .success();
    tag
}

#[test]
fn save_type_block_extracts_compiled_disk() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    let tag = build_and_compile_bootable(scratch.path(), &layout_dir);

    let disk = scratch.path().join("disk.img");
    umf()
        .args([
            "save",
            &tag,
            "--type",
            "block",
            "-o",
            disk.to_str().unwrap(),
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stderr(contains("Extracted bootable block"));

    // Read only the GPT header — the block is a multi-GiB sparse file.
    use std::io::Read;
    let mut head = [0u8; 520];
    std::fs::File::open(&disk)
        .expect("open disk")
        .read_exact(&mut head)
        .expect("read header");
    assert_eq!(&head[510..512], &[0x55, 0xAA], "protective MBR missing");
    assert_eq!(&head[512..520], b"EFI PART", "GPT signature missing");
}

#[test]
fn save_type_block_without_compile_errors() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    seed_kernel_image(&layout_dir, "imagilux/kernel-linux:7.0", "7.0");
    let tag = "example.invalid/bootable:uncompiled";
    umf()
        .args([
            "build",
            "--tag",
            tag,
            "--layout-dir",
            layout_dir.to_str().unwrap(),
            fixture("vm_minimal.umf").to_str().unwrap(),
        ])
        .assert()
        .success();

    let disk = scratch.path().join("disk.img");
    umf()
        .args([
            "save",
            tag,
            "--type",
            "block",
            "-o",
            disk.to_str().unwrap(),
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("no compiled block"));
}

#[test]
fn save_type_block_rejects_container() {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    let layout = ImageLayout::init(&layout_dir).expect("init layout");
    let tag = "example.invalid/container:save";
    let cfg = ImageConfig {
        umf_type: L0Kind::Container,
        container: ContainerConfig::default(),
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, tag).expect("emit container");

    let disk = scratch.path().join("disk.img");
    umf()
        .args([
            "save",
            tag,
            "--type",
            "block",
            "-o",
            disk.to_str().unwrap(),
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("only for a bootable image"));
}

#[test]
fn images_prune_reaps_the_block_cache() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let layout_dir = scratch.path().join("layout");
    let tag = build_and_compile_bootable(scratch.path(), &layout_dir);

    // Untag the image + prune: with the source image gone from the index, its
    // cached block is unreachable and gets reaped.
    umf()
        .args([
            "images",
            "--remove",
            &tag,
            "--prune",
            "--layout-dir",
            layout_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("block"));
}

/// Stage a synthetic single-arch container image whose OCI config records
/// `architecture = <oci_arch>` and a per-arch entrypoint marker, returning the
/// ref it was emitted under. Used to compose a multi-arch index without a
/// network or a real build.
#[cfg(test)]
fn stage_arch_image(layout_dir: &std::path::Path, oci_arch: &str) -> String {
    use umf_core::l0::L0Kind;
    use umf_oci::image::{ContainerConfig, ImageConfig, emit_image};
    use umf_oci::registry::ImageLayout;

    let layout = ImageLayout::open(layout_dir).expect("open layout");
    let ref_name = format!("example.invalid/app:{oci_arch}");
    let cfg = ImageConfig {
        architecture: oci_arch.to_string(),
        os: "linux".to_string(),
        container: ContainerConfig {
            entrypoint: Some(vec![format!("/bin/{oci_arch}-marker")]),
            ..ContainerConfig::default()
        },
        umf_type: L0Kind::Container,
        ..ImageConfig::default()
    };
    emit_image(&layout, &[], &cfg, &ref_name).expect("emit per-arch image");
    ref_name
}

/// End to end: stage two per-arch images, compose them into one index with
/// `umf index`, then `umf inspect --platform=…` and confirm each arch selects
/// its own child (the per-arch entrypoint marker + `architecture` field prove
/// which manifest was chosen). No network — everything is staged in a temp
/// layout. Covers the OCI-3 acceptance: build N arches → one index → introspect
/// selects per-arch.
#[test]
fn index_composes_multiarch_and_inspect_selects_per_arch() {
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    ImageLayout::init(layout_dir.path()).expect("init layout");

    let amd_ref = stage_arch_image(layout_dir.path(), "amd64");
    let arm_ref = stage_arch_image(layout_dir.path(), "arm64");
    let index_ref = "example.invalid/app:multi";

    // Compose the index from the two per-arch refs.
    umf()
        .args(["index", "--tag", index_ref, "--layout-dir"])
        .arg(layout_dir.path())
        .args([&amd_ref, &arm_ref])
        .assert()
        .success()
        .stdout(contains("Composed index"))
        .stdout(contains(index_ref))
        .stdout(contains("2 arches"));

    // Inspect the index selecting arm64 → arm64 child.
    let arm_json = inspect_index_json(layout_dir.path(), index_ref, Some("linux/arm64"));
    assert_eq!(arm_json["image"]["architecture"].as_str(), Some("arm64"));
    assert_eq!(
        arm_json["runtime"]["entrypoint"][0].as_str(),
        Some("/bin/arm64-marker"),
    );

    // ...and amd64 → amd64 child.
    let amd_json = inspect_index_json(layout_dir.path(), index_ref, Some("linux/amd64"));
    assert_eq!(amd_json["image"]["architecture"].as_str(), Some("amd64"));
    assert_eq!(
        amd_json["runtime"]["entrypoint"][0].as_str(),
        Some("/bin/amd64-marker"),
    );

    // The two selections must resolve to *different* child manifests.
    assert_ne!(
        arm_json["manifest"]["digest"].as_str(),
        amd_json["manifest"]["digest"].as_str(),
    );
}

/// Run `umf inspect --format=json` (optionally with `--platform`) against an
/// index in `layout_dir` and parse the report.
#[cfg(test)]
fn inspect_index_json(
    layout_dir: &std::path::Path,
    reference: &str,
    platform: Option<&str>,
) -> serde_json::Value {
    let mut cmd = umf();
    cmd.args(["inspect", "--format=json", "--layout-dir"])
        .arg(layout_dir)
        .arg(reference);
    if let Some(p) = platform {
        cmd.args(["--platform", p]);
    }
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).expect("inspect --format=json must be valid JSON")
}

/// `umf inspect` on an index without `--platform` falls back to the host arch.
/// On the (x86_64) CI host that selects the amd64 child; the test asserts the
/// selection is one of the advertised arches rather than erroring on the index.
#[test]
fn inspect_index_without_platform_selects_a_child() {
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    ImageLayout::init(layout_dir.path()).expect("init layout");
    let amd_ref = stage_arch_image(layout_dir.path(), "amd64");
    let arm_ref = stage_arch_image(layout_dir.path(), "arm64");
    let index_ref = "example.invalid/app:multi";
    umf()
        .args(["index", "--tag", index_ref, "--layout-dir"])
        .arg(layout_dir.path())
        .args([&amd_ref, &arm_ref])
        .assert()
        .success();

    let json = inspect_index_json(layout_dir.path(), index_ref, None);
    let arch = json["image"]["architecture"].as_str().expect("arch string");
    assert!(
        arch == "amd64" || arch == "arm64",
        "host-arch fallback should select an advertised child, got {arch}",
    );
}

/// `umf inspect --platform` for an arch the index doesn't carry fails with a
/// clear, listing-the-available-arches error rather than silently picking the
/// wrong one.
#[test]
fn inspect_index_missing_platform_errors_clearly() {
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    ImageLayout::init(layout_dir.path()).expect("init layout");
    let amd_ref = stage_arch_image(layout_dir.path(), "amd64");
    let index_ref = "example.invalid/app:amd-only";
    umf()
        .args(["index", "--tag", index_ref, "--layout-dir"])
        .arg(layout_dir.path())
        .args([&amd_ref])
        .assert()
        .success();

    umf()
        .args(["inspect", "--platform", "linux/arm64", "--layout-dir"])
        .arg(layout_dir.path())
        .arg(index_ref)
        .assert()
        .failure()
        .stderr(contains("no manifest for arm64"))
        .stderr(contains("linux/amd64"));
}

/// `umf index` requires at least one child ref (clap `required = true`).
#[test]
fn index_requires_at_least_one_child() {
    umf()
        .args(["index", "--tag", "example.invalid/x:multi"])
        .assert()
        .failure();
}

/// `umf index` on a ref absent from the layout fails with a build-it-first hint.
#[test]
fn index_errors_on_unknown_child() {
    use umf_oci::registry::ImageLayout;

    let layout_dir = tempfile::tempdir().expect("layout tempdir");
    ImageLayout::init(layout_dir.path()).expect("init layout");

    umf()
        .args(["index", "--tag", "example.invalid/x:multi", "--layout-dir"])
        .arg(layout_dir.path())
        .arg("example.invalid/does-not-exist:latest")
        .assert()
        .failure()
        .stderr(contains("child ref not found"));
}

#[test]
fn registry_add_list_remove_roundtrip() {
    // Isolate the config to a temp `$XDG_CONFIG_HOME` so the real one is untouched.
    let xdg = tempfile::tempdir().expect("xdg tempdir");

    // Empty: list reports docker.io-only.
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "list"])
        .assert()
        .success()
        .stdout(contains("docker.io only"));

    // Add two registries; the second add reports idempotency.
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "add", "registry.example.com"])
        .assert()
        .success()
        .stdout(contains("Added search registry: registry.example.com"));
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "add", "ghcr.io"])
        .assert()
        .success();
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "add", "registry.example.com"])
        .assert()
        .success()
        .stdout(contains("Already configured"));

    // List shows both in precedence order, then the implicit docker.io fallback.
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "list"])
        .assert()
        .success()
        .stdout(contains("1. registry.example.com"))
        .stdout(contains("2. ghcr.io"))
        .stdout(contains("docker.io (implicit fallback)"));

    // Remove the first; ghcr.io becomes the head of the list.
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "remove", "registry.example.com"])
        .assert()
        .success()
        .stdout(contains("Removed search registry: registry.example.com"));
    umf()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args(["registry", "list"])
        .assert()
        .success()
        .stdout(contains("1. ghcr.io"));
}
