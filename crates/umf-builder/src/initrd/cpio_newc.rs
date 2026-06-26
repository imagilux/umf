//! Minimal CPIO `newc` archive writer.
//!
//! Linux's initramfs decoder accepts CPIO archives in several formats; the
//! `newc` (SVR4-compatible) format is the universally supported one and
//! the only one we need to emit. The format is small enough to roll our
//! own emitter rather than pull in a dependency:
//!
//! ```text
//! +--------------------+
//! |   110-byte header  |  ASCII hex, fixed width
//! +--------------------+
//! |   filename + NUL   |  c_namesize bytes
//! +--------------------+
//! |   padding to 4B    |  relative to header start
//! +--------------------+
//! |   file data        |  c_filesize bytes (0 for dirs / symlink target
//! +--------------------+    for symlinks)
//! |   padding to 4B    |
//! +--------------------+
//! ```
//!
//! At the end of the stream we emit a synthetic entry with the magic name
//! `TRAILER!!!` and a zero filesize. The Linux decoder treats that name as
//! end-of-archive.
//!
//! Unix mode bits are passed through verbatim; only the high-nibble file
//! kind is set by us (`S_IFREG` for files, `S_IFDIR` for dirs, `S_IFLNK`
//! for symlinks). UID / GID default to 0; the caller can override if a
//! future use case needs it.

use std::path::PathBuf;

/// CPIO magic for newc (SVR4 without CRC). ASCII `"070701"`.
const NEWC_MAGIC: &[u8; 6] = b"070701";

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// One entry to be placed in the archive.
#[derive(Debug, Clone, Default)]
pub struct CpioEntry {
    /// Path inside the archive (relative — no leading `/`). Use `.` for
    /// the implicit root.
    pub path: PathBuf,
    /// What kind of node this is, and (for regular files / symlinks) the
    /// payload.
    pub kind: CpioKind,
    /// Unix permission bits (the lower 12). The file-kind nibble is
    /// derived from `kind`, not from this field — leave it as 0o755 or
    /// 0o644 etc.
    pub mode: u32,
    /// UID for the entry (defaults to 0 / root).
    pub uid: u32,
    /// GID for the entry (defaults to 0 / root).
    pub gid: u32,
    /// Modification time as a Unix epoch (defaults to 0 — reproducible).
    pub mtime: u32,
}

/// What a [`CpioEntry`] represents.
#[derive(Debug, Clone, Default)]
pub enum CpioKind {
    /// Regular file — payload is the file's bytes.
    File(Vec<u8>),
    /// Directory — no payload. (Default — matches the implicit root entry
    /// in a fresh [`CpioEntry`].)
    #[default]
    Directory,
    /// Symlink — payload is the link target (a path).
    Symlink(PathBuf),
}

/// Stateful builder accumulating entries, then emitting the archive bytes.
#[derive(Debug, Default)]
pub struct CpioWriter {
    entries: Vec<CpioEntry>,
}

impl CpioWriter {
    /// Empty archive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `entry` to the archive. Order matters — parents should be
    /// pushed before their children so a strict CPIO reader sees a
    /// consistent tree, though Linux's initramfs decoder is forgiving.
    pub fn push(&mut self, entry: CpioEntry) {
        self.entries.push(entry);
    }

    /// Consume the writer and emit the final archive bytes (CPIO newc, not
    /// compressed — the caller wraps in gzip / xz / etc.).
    pub fn finish(self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(4096);
        for (i, entry) in self.entries.iter().enumerate() {
            // ino starts at 1; ino 0 is conventionally invalid.
            let ino = (i as u32) + 1;
            write_entry(&mut out, ino, entry);
        }
        // Trailer.
        write_trailer(&mut out);
        out
    }
}

fn write_entry(out: &mut Vec<u8>, ino: u32, entry: &CpioEntry) {
    let (filetype, payload): (u32, Vec<u8>) = match &entry.kind {
        CpioKind::File(bytes) => (S_IFREG, bytes.clone()),
        CpioKind::Directory => (S_IFDIR, Vec::new()),
        CpioKind::Symlink(target) => (S_IFLNK, target.to_string_lossy().as_bytes().to_vec()),
    };

    let nlink = if matches!(entry.kind, CpioKind::Directory) {
        2 // POSIX convention: `.` + `..`
    } else {
        1
    };

    let mode = (filetype & S_IFMT) | (entry.mode & 0o7777);
    let name_bytes = entry.path.to_string_lossy().as_bytes().to_vec();
    // Include the null terminator in c_namesize.
    let name_len_with_nul = name_bytes.len() + 1;

    write_header(
        out,
        ino,
        mode,
        entry.uid,
        entry.gid,
        nlink,
        entry.mtime,
        payload.len() as u32,
        name_len_with_nul as u32,
    );
    out.extend_from_slice(&name_bytes);
    out.push(0); // NUL terminator
    pad_to_4(out);
    out.extend_from_slice(&payload);
    pad_to_4(out);
}

fn write_trailer(out: &mut Vec<u8>) {
    let name = b"TRAILER!!!";
    let name_len_with_nul = name.len() + 1;
    write_header(
        out,
        0, // ino
        0, // mode
        0, // uid
        0, // gid
        1, // nlink
        0, // mtime
        0, // filesize
        name_len_with_nul as u32,
    );
    out.extend_from_slice(name);
    out.push(0);
    pad_to_4(out);
}

#[allow(clippy::too_many_arguments)] // CPIO header fields are exactly these
fn write_header(
    out: &mut Vec<u8>,
    ino: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    filesize: u32,
    namesize: u32,
) {
    out.extend_from_slice(NEWC_MAGIC);
    push_hex(out, ino);
    push_hex(out, mode);
    push_hex(out, uid);
    push_hex(out, gid);
    push_hex(out, nlink);
    push_hex(out, mtime);
    push_hex(out, filesize);
    push_hex(out, 0); // devmajor
    push_hex(out, 0); // devminor
    push_hex(out, 0); // rdevmajor
    push_hex(out, 0); // rdevminor
    push_hex(out, namesize);
    push_hex(out, 0); // checksum (newc → always zero)
}

fn push_hex(out: &mut Vec<u8>, value: u32) {
    let s = format!("{value:08X}");
    out.extend_from_slice(s.as_bytes());
}

fn pad_to_4(out: &mut Vec<u8>) {
    let pad = (4 - (out.len() % 4)) % 4;
    for _ in 0..pad {
        out.push(0);
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
