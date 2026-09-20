//! Unit tests for the `filesystem` module.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use backhand::FilesystemReader;
use std::io::Cursor;
use tempfile::TempDir;

fn seed_basic_tree() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::create_dir_all(root.join("usr/bin")).unwrap();
    std::fs::write(root.join("etc/os-release"), b"NAME=\"Alpine Linux\"\n").unwrap();
    std::fs::write(root.join("usr/bin/hello"), b"#!/bin/sh\necho hi\n").unwrap();
    std::os::unix::fs::symlink("hello", root.join("usr/bin/hi")).expect("symlink");
    dir
}

#[test]
fn round_trip_basic_tree() {
    let dir = seed_basic_tree();
    let mut buf: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    let report = write_squashfs_from_dir(dir.path(), &mut cursor).expect("write");
    assert!(report.files >= 2, "expected files in report: {report:?}");
    assert!(report.symlinks >= 1, "expected symlinks: {report:?}");

    let mut rdr = Cursor::new(&buf);
    let fs = FilesystemReader::from_reader(&mut rdr).expect("read squashfs");
    let nodes: Vec<_> = fs.files().collect();
    let has = |path: &str| {
        nodes
            .iter()
            .any(|n| n.fullpath.to_string_lossy().as_ref() == path)
    };
    assert!(has("/etc/os-release"), "missing /etc/os-release");
    assert!(has("/usr/bin/hello"), "missing /usr/bin/hello");
    assert!(has("/usr/bin/hi"), "missing /usr/bin/hi (symlink)");
}

#[test]
fn symlinks_preserve_target() {
    let dir = seed_basic_tree();
    let mut buf: Vec<u8> = Vec::new();
    write_squashfs_from_dir(dir.path(), &mut Cursor::new(&mut buf)).expect("write");

    let mut rdr = Cursor::new(&buf);
    let fs = FilesystemReader::from_reader(&mut rdr).expect("read");
    let node = fs
        .files()
        .find(|n| n.fullpath.to_string_lossy() == "/usr/bin/hi")
        .expect("hi symlink");
    match &node.inner {
        backhand::InnerNode::Symlink(sym) => {
            assert_eq!(sym.link.to_string_lossy(), "hello");
        }
        other => panic!("expected symlink, got {other:?}"),
    }
}

#[test]
fn permissions_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let exec_path = dir.path().join("bin").join("script.sh");
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    {
        let mut f = std::fs::File::create(&exec_path).unwrap();
        f.write_all(b"#!/bin/sh\n").unwrap();
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }

    let mut buf: Vec<u8> = Vec::new();
    write_squashfs_from_dir(dir.path(), &mut Cursor::new(&mut buf)).expect("write");

    let mut rdr = Cursor::new(&buf);
    let fs = FilesystemReader::from_reader(&mut rdr).expect("read");
    let node = fs
        .files()
        .find(|n| n.fullpath.to_string_lossy() == "/bin/script.sh")
        .expect("script");
    assert_eq!(node.header.permissions & 0o7777, 0o755);
}

// ── Sparse copy ─────────────────────────────────────────────────────────────

/// A reader that hands back at most `chunk` bytes per call.
///
/// `Read::read` is allowed to return short. The copy stays correct because it
/// slices to what was read rather than assuming a full buffer — this pins that
/// property, since the obvious wrong version (treating the untouched tail of
/// the buffer as part of the chunk) would read stale bytes from the previous
/// iteration and corrupt the image.
struct Dribble<'a> {
    data: &'a [u8],
    pos: usize,
    chunk: usize,
}

impl Read for Dribble<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.data.len() - self.pos;
        let n = remaining.min(buf.len()).min(self.chunk);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The destination starts as a pre-sized zeroed partition, exactly as the real
/// one does (`set_len` on the disk file, then a `PartitionView` over it).
fn sink(len: usize) -> Cursor<Vec<u8>> {
    Cursor::new(vec![0u8; len])
}

#[test]
fn the_sparse_copy_reproduces_the_source_byte_for_byte() {
    // Data, a long hole, more data, a trailing hole — every transition the
    // loop has to handle.
    let mut src = Vec::new();
    src.extend_from_slice(&[0xAB; 100 * 1024]);
    src.extend_from_slice(&[0x00; 512 * 1024]);
    src.extend_from_slice(&[0xCD; 70 * 1024]);
    src.extend_from_slice(&[0x00; 256 * 1024]);

    let mut dst = sink(src.len());
    copy_skipping_holes(&mut Cursor::new(&src), &mut dst).expect("copy");

    assert_eq!(
        dst.into_inner(),
        src,
        "the copied partition must be identical to the produced image",
    );
}

#[test]
fn the_sparse_copy_writes_only_the_non_zero_bytes() {
    // The whole point: a mostly-empty ext4 image must not materialise every
    // block of the disk, or UMF loses "build output is always a sparse image".
    let mut src = vec![0u8; 1024 * 1024];
    src[..4096].fill(0xEE);

    let mut dst = sink(src.len());
    let written = copy_skipping_holes(&mut Cursor::new(&src), &mut dst).expect("copy");

    assert!(
        written < 200 * 1024,
        "expected roughly one chunk of writes for 4 KiB of data, wrote {written}",
    );
    assert_eq!(dst.into_inner(), src, "skipping holes must not lose data");
}

#[test]
fn the_sparse_copy_survives_short_reads() {
    // 7 bytes at a time: every buffer fill is partial, so a naive single
    // `read` would see mostly-zero buffers and punch holes through real data.
    let mut src = vec![0x5Au8; 200 * 1024];
    src[50 * 1024..150 * 1024].fill(0);

    let mut dst = sink(src.len());
    copy_skipping_holes(
        &mut Dribble {
            data: &src,
            pos: 0,
            chunk: 7,
        },
        &mut dst,
    )
    .expect("copy");

    assert_eq!(
        dst.into_inner(),
        src,
        "short reads must not corrupt the copy",
    );
}

#[test]
fn an_all_zero_source_writes_nothing_at_all() {
    let src = vec![0u8; 512 * 1024];
    let mut dst = sink(src.len());
    let written = copy_skipping_holes(&mut Cursor::new(&src), &mut dst).expect("copy");
    assert_eq!(written, 0, "an empty image should touch no blocks");
    assert_eq!(dst.into_inner(), src);
}

// ── Host-mkfs writers ───────────────────────────────────────────────────────

/// Superblock magic per filesystem, as `(offset, bytes)`.
///
/// Checked at the *partition's* offset 0 rather than just "the output is
/// non-empty": the failure this guards against is writing the right bytes to
/// the wrong place, or writing a different filesystem than the cmdline will
/// claim. Both produce a file that looks fine by length alone and does not
/// boot.
pub(crate) fn superblock_magic(fs: RootfsFs) -> (usize, &'static [u8]) {
    match fs {
        // "hsqs" — squashfs, little-endian, at offset 0.
        RootfsFs::Squashfs => (0, b"hsqs"),
        // s_magic 0xEF53, little-endian, at 0x38 within the superblock,
        // which itself lives at byte 1024.
        RootfsFs::Ext4 => (1024 + 0x38, &[0x53, 0xEF]),
        // EROFS_SUPER_MAGIC_V1 = 0xE0F5E1E2, little-endian, at byte 1024.
        RootfsFs::Erofs => (1024, &[0xE2, 0xE1, 0xF5, 0xE0]),
    }
}

/// Skip (loudly) when the host lacks the tool, unless the caller demands the
/// tool-backed paths actually run.
///
/// Mirrors `UMF_REQUIRE_PRIVILEGED` in the privileged lane: CI sets
/// `UMF_REQUIRE_MKFS=1` so a lane that lost `e2fsprogs` / `erofs-utils` fails
/// loudly instead of reporting green having tested neither writer.
fn mkfs_available_or_skip(fs: RootfsFs) -> bool {
    let Some(tool) = fs.host_mkfs() else {
        return true;
    };
    if tool_on_path(tool) {
        return true;
    }
    let required =
        std::env::var("UMF_REQUIRE_MKFS").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    assert!(
        !required,
        "UMF_REQUIRE_MKFS is set but `{tool}` is missing — this lane is supposed to \
         exercise the {fs} writer and would otherwise report green having tested nothing",
    );
    eprintln!("skipping {fs} writer test: `{tool}` not on PATH");
    false
}

#[test]
fn every_filesystem_writes_its_own_superblock_at_the_partition_start() {
    let dir = seed_basic_tree();
    // Comfortably larger than the tree, and a multiple of a sane block size.
    const CAPACITY: u64 = 16 * 1024 * 1024;

    for fs in RootfsFs::ALL {
        if !mkfs_available_or_skip(fs) {
            continue;
        }
        let mut dst = sink(CAPACITY as usize);
        let report =
            write_rootfs_from_dir(fs, dir.path(), &mut dst, CAPACITY).expect("write rootfs");

        assert_eq!(report.fs, fs, "report must name the filesystem written");
        assert!(report.image_bytes > 0, "{fs}: empty image");
        assert!(
            report.image_bytes <= CAPACITY,
            "{fs}: image {} exceeds capacity {CAPACITY}",
            report.image_bytes,
        );

        let bytes = dst.into_inner();
        let (offset, magic) = superblock_magic(fs);
        assert_eq!(
            &bytes[offset..offset + magic.len()],
            magic,
            "{fs}: superblock magic missing at offset {offset} — wrong filesystem, \
             or written at the wrong offset within the partition",
        );
    }
}

/// Per-node counts are reported only for the filesystem this crate walks
/// itself. Inventing them for the host tools would describe a traversal that
/// never happened.
#[test]
fn only_the_in_process_writer_reports_per_node_counts() {
    let dir = seed_basic_tree();
    const CAPACITY: u64 = 16 * 1024 * 1024;

    let mut dst = sink(CAPACITY as usize);
    let squash = write_rootfs_from_dir(RootfsFs::Squashfs, dir.path(), &mut dst, CAPACITY)
        .expect("squashfs");
    let nodes = squash.nodes.expect("squashfs must report node counts");
    assert!(nodes.files >= 2, "expected files: {nodes:?}");
    assert!(nodes.symlinks >= 1, "expected symlinks: {nodes:?}");

    for fs in [RootfsFs::Ext4, RootfsFs::Erofs] {
        if !mkfs_available_or_skip(fs) {
            continue;
        }
        let mut dst = sink(CAPACITY as usize);
        let report = write_rootfs_from_dir(fs, dir.path(), &mut dst, CAPACITY).expect("write");
        assert!(
            report.nodes.is_none(),
            "{fs} must not report counts for a traversal it did not perform",
        );
    }
}

/// An image that does not fit is an error, never a truncated partition. A
/// truncated filesystem mounts far enough to look plausible and then fails at
/// the first read past the cut.
#[test]
fn an_image_larger_than_the_partition_is_rejected() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("big")).unwrap();
    // Genuinely incompressible content, so squashfs cannot shrink it under the
    // capacity. A simple arithmetic pattern will not do — the obvious
    // `i * K` low-byte sequence repeats every 256 bytes and squashfs packs
    // 2 MiB of it into 8 KiB, which quietly turns this test green for the
    // wrong reason. xorshift64 has no such short period.
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let filler: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xff) as u8
        })
        .collect();
    std::fs::write(dir.path().join("big/blob"), &filler).unwrap();

    const TINY: u64 = 256 * 1024;
    let mut dst = sink(TINY as usize);
    let err = write_rootfs_from_dir(RootfsFs::Squashfs, dir.path(), &mut dst, TINY)
        .expect_err("a 2 MiB tree must not fit a 256 KiB partition");
    assert!(
        matches!(err, FilesystemError::ImageTooLarge { .. }),
        "expected ImageTooLarge, got {err:?}",
    );
}

#[test]
fn a_missing_tool_names_the_package_and_the_escape_hatch() {
    let err = FilesystemError::MkfsUnavailable {
        tool: "mkfs.ext4",
        fs: RootfsFs::Ext4,
        package: "e2fsprogs",
    };
    let text = err.to_string();
    assert!(text.contains("mkfs.ext4"), "names the tool: {text}");
    assert!(text.contains("e2fsprogs"), "names the package: {text}");
    assert!(
        text.contains("--fs squashfs"),
        "must offer the no-tooling alternative: {text}",
    );
}

/// `tool_on_path`, re-exported for the end-to-end tests in `image::tests`,
/// which need the same "is this host able to write that filesystem" answer.
pub(crate) fn tool_is_available(tool: &str) -> bool {
    tool_on_path(tool)
}
