//! End-to-end validation of the erofs lower-layer path:
//! encode two OCI layers to erofs, mount them as a stacked overlayfs
//! lower set, and assert the merged view is correct — including OCI
//! whiteouts and opaque directories (converted by `mkfs.erofs --aufs`
//! to overlayfs char-device whiteouts + opaque xattrs).
//!
//! This is the test that proves per-layer erofs images stack the same
//! way the merge-unpack path does. Gated on root + a loadable erofs
//! kernel module + `mkfs.erofs` — skipped (not failed) otherwise.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write as _;
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;
use umf_engine::erofs::{MountedErofsLayers, mount_available};
use umf_engine::overlay::Overlay;
use umf_oci::erofs::{encoder_available, ensure_layer_erofs};
use umf_oci::registry::ImageLayout;
use umf_oci::registry::layout::sha256_digest;

/// One tar entry to synthesise into a layer.
enum Entry<'a> {
    Dir(&'a str),
    File(&'a str, &'a [u8]),
}

/// Build a gzipped tar from `entries`, returning `(gzip_bytes, diff_id)`.
/// `diff_id` is the sha256 of the *uncompressed* tar, matching OCI's
/// `rootfs.diff_ids`.
fn gz_layer(entries: &[Entry<'_>]) -> (Vec<u8>, String) {
    let mut tar_bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        for e in entries {
            match e {
                Entry::Dir(path) => {
                    let mut h = tar::Header::new_gnu();
                    h.set_path(path).expect("dir path");
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(0o755);
                    h.set_size(0);
                    h.set_cksum();
                    b.append(&h, std::io::empty()).expect("append dir");
                }
                Entry::File(path, contents) => {
                    // Set the typeflag to Regular explicitly: a `new_gnu`
                    // header defaults the typeflag to NUL, which
                    // `mkfs.erofs --tar` silently drops rather than
                    // materialising as a file.
                    let mut h = tar::Header::new_gnu();
                    h.set_path(path).expect("file path");
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_mode(0o644);
                    h.set_size(contents.len() as u64);
                    h.set_cksum();
                    b.append(&h, *contents).expect("append file");
                }
            }
        }
        b.finish().expect("finish tar");
    }
    let diff_id = sha256_digest(&tar_bytes);
    let mut gz = Vec::new();
    let mut enc = GzEncoder::new(&mut gz, Compression::default());
    enc.write_all(&tar_bytes).expect("gz write");
    enc.finish().expect("gz finish");
    (gz, diff_id)
}

/// Encode one gzip layer into the layout's erofs cache, returning the
/// erofs path.
fn encode(layout: &ImageLayout, gz: &[u8], diff_id: &str) -> std::path::PathBuf {
    let blob_digest = layout.write_blob(gz).expect("write blob");
    ensure_layer_erofs(layout, &blob_digest, diff_id).expect("encode erofs")
}

#[test]
fn stacked_erofs_lowers_apply_whiteouts_and_opaque_dirs() {
    // This test also mounts an *overlay* over the erofs lowers, which
    // rootless needs fuse-overlayfs — so unlike the plain erofs mount
    // test (which runs rootless via erofsfuse), gate this one on root +
    // kernel erofs, where `mount -t overlay` is unconditionally available.
    if !nix::unistd::Uid::current().is_root() || !mount_available() || !encoder_available() {
        eprintln!(
            "skipping erofs overlay test: needs root + erofs kernel module + mkfs.erofs \
             (run via `sudo <test-bin>`)"
        );
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let layout = ImageLayout::init(dir.path()).expect("init layout");

    // Lower layer: keep + remove files under /etc, and a populated /data.
    let (lower_gz, lower_diff) = gz_layer(&[
        Entry::Dir("etc/"),
        Entry::File("etc/keep", b"keep\n"),
        Entry::File("etc/remove", b"will-be-removed\n"),
        Entry::Dir("data/"),
        Entry::File("data/old.txt", b"old\n"),
    ]);

    // Upper layer: whiteout etc/remove, add etc/new, make /data opaque
    // and drop a fresh file into it. `.wh.remove` and `.wh..wh..opq` are
    // the OCI/aufs whiteout markers `mkfs.erofs --aufs` converts.
    let (upper_gz, upper_diff) = gz_layer(&[
        Entry::Dir("etc/"),
        Entry::File("etc/.wh.remove", b""),
        Entry::File("etc/new", b"new\n"),
        Entry::Dir("data/"),
        Entry::File("data/.wh..wh..opq", b""),
        Entry::File("data/fresh.txt", b"fresh\n"),
    ]);

    let lower_erofs = encode(&layout, &lower_gz, &lower_diff);
    let upper_erofs = encode(&layout, &upper_gz, &upper_diff);

    // Mount newest → oldest (overlay lower order is top → bottom).
    let mounted = MountedErofsLayers::mount(&[upper_erofs, lower_erofs]).expect("mount erofs");
    let overlay = Overlay::mount(&mounted.mountpoints()).expect("overlay mount");
    let merged = overlay.merged();

    let exists = |rel: &str| Path::new(merged).join(rel).exists();
    let read = |rel: &str| std::fs::read_to_string(Path::new(merged).join(rel)).expect(rel);

    // Surviving lower file + new upper file.
    assert_eq!(read("etc/keep"), "keep\n", "lower file should survive");
    assert_eq!(read("etc/new"), "new\n", "upper file should appear");
    // Whiteout removed the lower file.
    assert!(!exists("etc/remove"), "whiteout should hide etc/remove");
    // Opaque dir hid all lower entries; only the upper's fresh file remains.
    assert!(
        !exists("data/old.txt"),
        "opaque dir should hide lower data/old.txt"
    );
    assert_eq!(read("data/fresh.txt"), "fresh\n", "upper data file present");
}
